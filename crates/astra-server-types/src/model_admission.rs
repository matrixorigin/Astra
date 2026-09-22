//! Safe wire projection for batched child-model preflight.
//!
//! This is a current-state check, not a durable authorization grant. Each
//! inference request still passes Server execution admission.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionSlotV1 {
    pub offering_id: String,
    /// Serialized `ReasoningSelection`; Server validates the exact type.
    pub reasoning: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionRequestV1 {
    pub slots: Vec<ModelAdmissionSlotV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionResultV1 {
    pub offering_id: String,
    pub reasoning: Value,
    pub model_name: String,
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionResponseV1 {
    pub slots: Vec<ModelAdmissionResultV1>,
}
