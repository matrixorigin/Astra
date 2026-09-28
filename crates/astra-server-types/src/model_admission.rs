//! Safe wire projection for batched child-model preflight.
//!
//! This is a current-state check, not a durable authorization grant. Each
//! inference request still passes Server execution admission.

use astra_turn_types::ModelSelector;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionSlotV1 {
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    pub selector: ModelSelector,
    /// Serialized `ReasoningSelection`; Server validates the exact type.
    pub reasoning: Value,
    /// Reuse parent reasoning only when name resolution yields this exact
    /// Offering. This avoids resolving a configured name in one request and
    /// issuing a second admission request just to decide inheritance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_reasoning: Option<ModelAdmissionReasoningInheritanceV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAdmissionReasoningInheritanceV1 {
    pub offering_id: String,
    /// Serialized `ReasoningSelection` inherited from this exact parent Offering.
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
    pub max_output_tokens: Option<u32>,
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
