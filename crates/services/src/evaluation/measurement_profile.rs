//! Versioned requirements, independent of the availability of metric collectors.

use serde::{Deserialize, Serialize};

/// The version fixes the meaning and order of these requirements. Adding or
/// reinterpreting a metric requires a new variant, never changing this list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeasurementProfile {
    #[serde(rename = "instruction-only.v1")]
    InstructionOnlyV1,
}

impl MeasurementProfile {
    pub fn requirements(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        match self {
            Self::InstructionOnlyV1 => &[
                ("task", "task_success", "boolean"),
                ("tool", "tool_calls", "calls"),
                ("tool", "tool_validity_rate", "ratio"),
                ("context", "context_snapshot_match", "boolean"),
                ("provider", "provider_binding_match", "boolean"),
                ("provider", "provider_fallback_count", "calls"),
                ("safety", "policy_violation_count", "violations"),
                ("reliability", "run_completed", "boolean"),
                ("cost", "prompt_tokens", "tokens"),
                ("cost", "completion_tokens", "tokens"),
                ("cost", "latency_ms", "milliseconds"),
                ("cost", "estimated_cost_usd", "USD"),
            ],
        }
    }
}
