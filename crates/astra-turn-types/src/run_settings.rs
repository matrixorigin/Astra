//! Typed user choices captured at the boundary of a model round.
use crate::{ModelSelection, PermissionMode, ReasoningRequirement};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPermissionModeRequest {
    pub expected_session_id: String,
    pub request_id: String,
    pub mode: PermissionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPermissionModeSelection {
    pub request_id: String,
    pub mode: PermissionMode,
    /// Durable index of the requested event, not a client-supplied counter.
    pub revision: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPermissionModeApplied {
    pub selection: RunPermissionModeSelection,
    pub round_index: u32,
    pub owner_generation: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPermissionModeSnapshot {
    pub requested: Option<RunPermissionModeSelection>,
    pub applied: Option<RunPermissionModeApplied>,
}

/// Request an exact Offering for a running root task. This carries no
/// provider credentials or route authority; the execution owner admits it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelRequest {
    pub expected_session_id: String,
    pub request_id: String,
    pub model: ModelSelection,
    pub reasoning: ReasoningRequirement,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelSelection {
    pub request_id: String,
    /// Durable event index assigned by the owner of the run journal.
    pub revision: i64,
    pub model: ModelSelection,
    pub reasoning: ReasoningRequirement,
}

/// A terminal result for an exact model selection. Transport uncertainty is
/// not a rejection and remains pending until the journal resolves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunModelOutcomeKind {
    Applied {
        model: ModelSelection,
        reasoning: ReasoningRequirement,
    },
    Rejected {
        kind: astra_core::ErrorKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
    },
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelOutcome {
    pub selection: RunModelSelection,
    pub owner_generation: u64,
    pub round_index: u32,
    pub result: RunModelOutcomeKind,
}

/// Safe client projection; resolved routes and source identity remain private.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunModelSnapshot {
    pub requested: Option<RunModelSelection>,
    pub outcome: Option<RunModelOutcome>,
    pub applied: Option<RunModelOutcome>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choice_uses_typed_reasoning_without_execution_material() {
        let request = RunModelRequest {
            expected_session_id: "session".into(),
            request_id: "request".into(),
            model: ModelSelection {
                offering_id: "offering".into(),
            },
            reasoning: ReasoningRequirement::ModelDefault,
        };
        let mut wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["reasoning"]["mode"], "model_default");
        assert_eq!(
            serde_json::from_value::<RunModelRequest>(wire.clone()).unwrap(),
            request
        );
        wire["api_key"] = "not-accepted".into();
        assert!(serde_json::from_value::<RunModelRequest>(wire).is_err());
    }
}
