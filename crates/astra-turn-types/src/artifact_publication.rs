//! A report's publication result is distinct from the outcome of its run.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactPublicationResult {
    Published {
        handle: String,
    },
    Unavailable {
        reason_code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPublicationV1 {
    pub schema_version: u16,
    pub run_id: String,
    pub turn_id: String,
    pub execution_owner_generation: u64,
    pub artifact_type: String,
    pub recorded: bool,
    #[serde(flatten)]
    pub result: ArtifactPublicationResult,
}

impl ArtifactPublicationV1 {
    pub fn is_valid(&self) -> bool {
        self.schema_version == 1
            && !self.run_id.is_empty()
            && self.run_id.len() <= 256
            && !self.turn_id.is_empty()
            && self.turn_id.len() <= 256
            && self.artifact_type == "explain_analyze_snapshot"
            && match &self.result {
                ArtifactPublicationResult::Published { handle } => handle
                    .strip_prefix("artifact://session/explain-analyze/")
                    .is_some_and(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())),
                ArtifactPublicationResult::Unavailable {
                    reason_code,
                    message,
                } => {
                    !reason_code.is_empty()
                        && reason_code.len() <= 64
                        && !message.is_empty()
                        && message.len() <= 512
                        && !message.chars().any(char::is_control)
                }
            }
    }

    pub fn user_notice(&self) -> String {
        match &self.result {
            ArtifactPublicationResult::Published { handle } => {
                format!("Explain report saved on server · available to the agent\n{handle}")
            }
            ArtifactPublicationResult::Unavailable { message, .. } => {
                format!("Explain report could not be saved on server · {message}")
            }
        }
    }

    pub fn to_wire(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("serializable publication result");
        value["type"] = "artifact_publication".into();
        value
    }

    pub fn from_wire(value: &serde_json::Value) -> Result<Self, &'static str> {
        if let Some(index) = value.get("index")
            && !index
                .as_u64()
                .is_some_and(|index| index <= crate::EXPLAIN_ANALYZE_MAX_SAFE_INTEGER)
        {
            return Err("invalid artifact publication replay index");
        }
        if value.get("type").and_then(serde_json::Value::as_str) != Some("artifact_publication") {
            return Err("unexpected artifact publication event type");
        }
        let mut payload = value.clone();
        let object = payload
            .as_object_mut()
            .ok_or("invalid publication object")?;
        object.remove("type");
        object.remove("index");
        let result: Self =
            serde_json::from_value(payload).map_err(|_| "invalid publication result")?;
        if !result.is_valid() {
            return Err("invalid publication identity or result");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_round_trip_preserves_success_and_unrecorded_failure() {
        for result in [
            ArtifactPublicationResult::Published {
                handle: format!("artifact://session/explain-analyze/{}", "a".repeat(64)),
            },
            ArtifactPublicationResult::Unavailable {
                reason_code: "storage_failed".into(),
                message: "Report storage failed.".into(),
            },
        ] {
            for recorded in [true, false] {
                let outcome = ArtifactPublicationV1 {
                    schema_version: 1,
                    run_id: "run".into(),
                    turn_id: "turn-1".into(),
                    execution_owner_generation: 1,
                    artifact_type: "explain_analyze_snapshot".into(),
                    recorded,
                    result: result.clone(),
                };
                assert_eq!(
                    ArtifactPublicationV1::from_wire(&outcome.to_wire()),
                    Ok(outcome)
                );
            }
        }
    }
}
