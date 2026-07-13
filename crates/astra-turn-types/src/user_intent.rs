use serde::{Deserialize, Serialize};

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
