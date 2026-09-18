use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const USER_TURN_SEMANTICS_FIELD: &str = "_astra_user_turn_semantics";
pub const USER_TURN_SEMANTICS_SCHEMA_VERSION: u8 = 1;

/// Judge-owned relationship between one user turn and the session objective.
///
/// The exact user message remains the content authority. This enum records
/// only how that message changes the objective, so projections never need to
/// infer lifecycle state from words such as "continue", "ok", or their
/// translations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveRelation {
    /// The judge could not establish a reliable relationship.
    #[default]
    Unknown,
    /// The user acknowledged the prior result without changing the objective.
    Acknowledge,
    /// The user asked to proceed without changing the objective.
    Continue,
    /// The user added a requirement or scope item to the current objective.
    Refine,
    /// The user corrected the current understanding or approach.
    Correct,
    /// The user started or explicitly replaced the objective.
    Replace,
}

impl ObjectiveRelation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Acknowledge => "acknowledge",
            Self::Continue => "continue",
            Self::Refine => "refine",
            Self::Correct => "correct",
            Self::Replace => "replace",
        }
    }

    #[must_use]
    pub const fn reanchors_current_objective(self) -> bool {
        matches!(self, Self::Correct | Self::Replace)
    }

    #[must_use]
    pub const fn updates_objective_context(self) -> bool {
        matches!(self, Self::Refine | Self::Correct | Self::Replace)
    }

    /// Canonical prompt label for typed objective projections.
    ///
    /// Keeping this vocabulary on the producer-owned enum prevents CLI and
    /// server resume surfaces from rendering the same state differently.
    #[must_use]
    pub const fn objective_context_label(self) -> &'static str {
        match self {
            Self::Replace => "objective",
            Self::Refine => "refinement",
            Self::Correct => "correction",
            Self::Unknown => "initial objective (judge unavailable)",
            Self::Acknowledge | Self::Continue => "unchanged objective",
        }
    }
}

/// Semantic class of explicit user feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserFeedbackKind {
    Approval,
    Correction,
    Clarification,
    Requirement,
    Preference,
}

impl UserFeedbackKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Correction => "correction",
            Self::Clarification => "clarification",
            Self::Requirement => "requirement",
            Self::Preference => "preference",
        }
    }
}

/// Part of the product behavior the feedback addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserFeedbackTarget {
    Objective,
    Scope,
    Approach,
    Output,
    Verification,
    General,
}

impl UserFeedbackTarget {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Objective => "objective",
            Self::Scope => "scope",
            Self::Approach => "approach",
            Self::Output => "output",
            Self::Verification => "verification",
            Self::General => "general",
        }
    }
}

/// Typed interpretation of feedback. The original user message is deliberately
/// not copied into this structure; it remains adjacent canonical content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserFeedback {
    pub kind: UserFeedbackKind,
    pub target: UserFeedbackTarget,
}

/// Confidence in one observational dimension, not a calibrated probability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssessmentConfidence {
    #[default]
    Unknown,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseSatisfaction {
    #[default]
    Unknown,
    Satisfied,
    Mixed,
    Dissatisfied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackResponseRelation {
    #[default]
    Unknown,
    PreviousResponse,
    EarlierOrMultiple,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskDifficulty {
    #[default]
    Unknown,
    Easy,
    Moderate,
    Difficult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskUrgency {
    #[default]
    Unknown,
    Normal,
    Urgent,
}

/// Observational judge output, never an authorization or a correctness verdict.
/// Each dimension has its own uncertainty; missing evidence stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TurnAssessment {
    pub satisfaction: ResponseSatisfaction,
    pub satisfaction_confidence: AssessmentConfidence,
    pub feedback_relation: FeedbackResponseRelation,
    pub difficulty: TaskDifficulty,
    pub difficulty_confidence: AssessmentConfidence,
    pub urgency: TaskUrgency,
    pub urgency_confidence: AssessmentConfidence,
}

/// Runtime-owned reference to the judged history snapshot. The root covers the
/// content prefix ending at the target, excluding mutable semantic annotations,
/// so repeated text and divergent content branches differ.
/// Resolve only against that exact snapshot; never guess after compaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackResponseReference {
    pub prefix_root: String,
    pub message_count: usize,
}

impl FeedbackResponseReference {
    /// Bind normalized canonical content, excluding the optional semantic
    /// annotations that display-history persistence may carry forward later.
    #[must_use]
    pub fn from_canonical_prefix(mut messages: Vec<Value>) -> Self {
        for message in &mut messages {
            if let Some(object) = message.as_object_mut() {
                object.remove(USER_TURN_SEMANTICS_FIELD);
            }
        }
        Self {
            prefix_root: crate::canonical_conversation_root(&messages),
            message_count: messages.len(),
        }
    }
}

/// Persisted semantics for a canonical user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserTurnSemantics {
    pub schema_version: u8,
    pub objective_relation: ObjectiveRelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<UserFeedback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assessment: Option<TurnAssessment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_response: Option<FeedbackResponseReference>,
}

/// Invalid producer-owned metadata is distinct from metadata that is absent.
/// Resume and persistence boundaries can therefore degrade explicitly instead
/// of silently treating a corrupt canonical message as an unjudged turn.
#[derive(Debug, thiserror::Error)]
pub enum UserTurnSemanticsError {
    #[error("user-turn semantics metadata belongs to a non-user message (role={role:?})")]
    InvalidOwner { role: Option<String> },
    #[error("malformed user-turn semantics metadata: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("unsupported user-turn semantics schema version {found}; expected {expected}")]
    UnsupportedSchemaVersion { found: u8, expected: u8 },
}

impl UserTurnSemantics {
    #[must_use]
    pub const fn new(
        objective_relation: ObjectiveRelation,
        feedback: Option<UserFeedback>,
    ) -> Self {
        Self {
            schema_version: USER_TURN_SEMANTICS_SCHEMA_VERSION,
            objective_relation,
            feedback,
            assessment: None,
            feedback_response: None,
        }
    }
}

/// Attach judge-owned semantics to a canonical user message.
///
/// Returns false for non-object/non-user messages instead of manufacturing a
/// second message identity.
pub fn mark_user_turn_semantics(message: &mut Value, semantics: UserTurnSemantics) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(object) = message.as_object_mut() else {
        return false;
    };
    object.insert(USER_TURN_SEMANTICS_FIELD.to_string(), json!(semantics));
    true
}

pub fn user_turn_semantics(
    message: &Value,
) -> Result<Option<UserTurnSemantics>, UserTurnSemanticsError> {
    let Some(raw_semantics) = message.get(USER_TURN_SEMANTICS_FIELD) else {
        return Ok(None);
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return Err(UserTurnSemanticsError::InvalidOwner {
            role: message
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let semantics: UserTurnSemantics =
        serde_json::from_value(raw_semantics.clone()).map_err(UserTurnSemanticsError::Malformed)?;
    if semantics.schema_version != USER_TURN_SEMANTICS_SCHEMA_VERSION {
        return Err(UserTurnSemanticsError::UnsupportedSchemaVersion {
            found: semantics.schema_version,
            expected: USER_TURN_SEMANTICS_SCHEMA_VERSION,
        });
    }
    Ok(Some(semantics))
}

/// How user input accepted while a run is active must be delivered.
///
/// Only the behavior implemented end-to-end is represented. New delivery
/// classes belong here only after the runtime, durable API, and user-facing
/// action all consume the same semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserIntentDelivery {
    GuideCurrentRun,
}

/// Cross-surface lifecycle checkpoints for a user intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserIntentStatus {
    AcceptedLocal,
    AcceptedRemote,
    Applied,
    /// The run reached a terminal boundary before applying the intent. The
    /// server has durably relinquished delivery ownership; interactive
    /// clients may restore the exact input as an unsent draft.
    Returned,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_reference_survives_annotation_carry_forward_but_not_branch_changes() {
        let mut prefix = vec![
            json!({"role":"user","content":"request"}),
            json!({"role":"assistant","content":"answer"}),
        ];
        let reference = FeedbackResponseReference::from_canonical_prefix(prefix.clone());
        mark_user_turn_semantics(
            &mut prefix[0],
            UserTurnSemantics::new(ObjectiveRelation::Replace, None),
        );
        assert_eq!(
            FeedbackResponseReference::from_canonical_prefix(prefix.clone()),
            reference
        );
        prefix[0]["content"] = json!("different branch");
        assert_ne!(
            FeedbackResponseReference::from_canonical_prefix(prefix),
            reference
        );
    }

    #[test]
    fn historical_semantics_remain_readable_and_assessments_round_trip() {
        let mut message = json!({"role":"user","content":"feedback",
            USER_TURN_SEMANTICS_FIELD: {"schema_version":1,"objective_relation":"unknown"}});
        let mut semantics = user_turn_semantics(&message).unwrap().unwrap();
        assert_eq!(semantics.assessment, None);
        assert_eq!(semantics.feedback_response, None);
        semantics.assessment = Some(TurnAssessment {
            satisfaction: ResponseSatisfaction::Mixed,
            satisfaction_confidence: AssessmentConfidence::Low,
            feedback_relation: FeedbackResponseRelation::PreviousResponse,
            difficulty: TaskDifficulty::Difficult,
            difficulty_confidence: AssessmentConfidence::High,
            ..Default::default()
        });
        semantics.feedback_response = Some(FeedbackResponseReference {
            prefix_root: crate::canonical_conversation_root(&[
                json!({"role":"assistant","content":"prior"}),
            ]),
            message_count: 1,
        });
        assert!(mark_user_turn_semantics(&mut message, semantics.clone()));
        assert_eq!(user_turn_semantics(&message).unwrap(), Some(semantics));
    }

    #[test]
    fn wire_values_are_stable_and_typed() {
        assert_eq!(
            serde_json::to_value(UserIntentDelivery::GuideCurrentRun).unwrap(),
            serde_json::json!("guide_current_run")
        );
        assert_eq!(
            serde_json::to_value(UserIntentStatus::AcceptedRemote).unwrap(),
            serde_json::json!("accepted_remote")
        );
        assert_eq!(
            serde_json::to_value(UserIntentStatus::Returned).unwrap(),
            serde_json::json!("returned")
        );
    }

    #[test]
    fn user_turn_semantics_round_trip_without_text_classification() {
        let semantics = UserTurnSemantics::new(
            ObjectiveRelation::Correct,
            Some(UserFeedback {
                kind: UserFeedbackKind::Correction,
                target: UserFeedbackTarget::Approach,
            }),
        );
        let mut message = json!({"role": "user", "content": "arbitrary text"});

        assert!(mark_user_turn_semantics(&mut message, semantics.clone()));
        assert_eq!(user_turn_semantics(&message).unwrap(), Some(semantics));
    }

    #[test]
    fn semantics_cannot_claim_non_user_messages() {
        let semantics = UserTurnSemantics::new(ObjectiveRelation::Replace, None);
        let mut assistant = json!({"role": "assistant", "content": "done"});

        assert!(!mark_user_turn_semantics(&mut assistant, semantics.clone()));
        assistant[USER_TURN_SEMANTICS_FIELD] = json!(semantics);
        assert!(matches!(
            user_turn_semantics(&assistant),
            Err(UserTurnSemanticsError::InvalidOwner { .. })
        ));
    }

    #[test]
    fn malformed_and_unsupported_semantics_are_not_reported_as_absent() {
        let malformed = json!({
            "role": "user",
            "content": "repair it",
            (USER_TURN_SEMANTICS_FIELD): {"schema_version": "one"}
        });
        assert!(matches!(
            user_turn_semantics(&malformed),
            Err(UserTurnSemanticsError::Malformed(_))
        ));

        let unsupported = json!({
            "role": "user",
            "content": "repair it",
            (USER_TURN_SEMANTICS_FIELD): {
                "schema_version": USER_TURN_SEMANTICS_SCHEMA_VERSION + 1,
                "objective_relation": "replace"
            }
        });
        assert!(matches!(
            user_turn_semantics(&unsupported),
            Err(UserTurnSemanticsError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn objective_context_labels_have_one_typed_owner() {
        assert_eq!(
            ObjectiveRelation::Unknown.objective_context_label(),
            "initial objective (judge unavailable)"
        );
        assert_eq!(
            ObjectiveRelation::Correct.objective_context_label(),
            "correction"
        );
    }
}
