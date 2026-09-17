//! Typed user control of the permission policy captured by a model round.
use crate::PermissionMode;
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
