use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub const TURN_MESSAGE_PROVENANCE_FIELD: &str = "_astra_turn_provenance";
pub const TURN_MESSAGE_PROVENANCE_SCHEMA_VERSION: u8 = 1;

/// Producer-owned identity for a conversational message created by one
/// active turn chain. This transport metadata survives context optimization
/// so the runtime can locate the current-turn suffix, then is removed before
/// provider delivery and canonical persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnMessageProvenanceV1 {
    pub schema_version: u8,
    pub turn_chain_id: String,
}

#[derive(Debug, Error)]
pub enum TurnMessageProvenanceError {
    #[error("turn provenance belongs to a non-conversational message (role={role:?})")]
    InvalidOwner { role: Option<String> },
    #[error("malformed turn provenance: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("unsupported turn provenance schema version {found}; expected {expected}")]
    UnsupportedSchemaVersion { found: u8, expected: u8 },
    #[error("turn provenance has an empty turn_chain_id")]
    EmptyTurnChainId,
}

fn is_conversational_role(role: Option<&str>) -> bool {
    matches!(role, Some("user" | "assistant" | "tool"))
}

/// Attach turn-chain identity without deriving it from message text.
/// Returns `false` for malformed/non-conversational messages or an empty id.
pub fn mark_turn_message(message: &mut Value, turn_chain_id: &str) -> bool {
    if turn_chain_id.is_empty()
        || !is_conversational_role(message.get("role").and_then(Value::as_str))
    {
        return false;
    }
    let Some(object) = message.as_object_mut() else {
        return false;
    };
    object.insert(
        TURN_MESSAGE_PROVENANCE_FIELD.to_string(),
        json!(TurnMessageProvenanceV1 {
            schema_version: TURN_MESSAGE_PROVENANCE_SCHEMA_VERSION,
            turn_chain_id: turn_chain_id.to_string(),
        }),
    );
    true
}

pub fn turn_message_provenance(
    message: &Value,
) -> Result<Option<TurnMessageProvenanceV1>, TurnMessageProvenanceError> {
    let Some(raw) = message.get(TURN_MESSAGE_PROVENANCE_FIELD) else {
        return Ok(None);
    };
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .map(str::to_string);
    if !is_conversational_role(role.as_deref()) {
        return Err(TurnMessageProvenanceError::InvalidOwner { role });
    }
    let provenance: TurnMessageProvenanceV1 =
        serde_json::from_value(raw.clone()).map_err(TurnMessageProvenanceError::Malformed)?;
    if provenance.schema_version != TURN_MESSAGE_PROVENANCE_SCHEMA_VERSION {
        return Err(TurnMessageProvenanceError::UnsupportedSchemaVersion {
            found: provenance.schema_version,
            expected: TURN_MESSAGE_PROVENANCE_SCHEMA_VERSION,
        });
    }
    if provenance.turn_chain_id.is_empty() {
        return Err(TurnMessageProvenanceError::EmptyTurnChainId);
    }
    Ok(Some(provenance))
}

pub fn clear_turn_message_provenance(message: &mut Value) {
    if let Some(object) = message.as_object_mut() {
        object.remove(TURN_MESSAGE_PROVENANCE_FIELD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_round_trips_only_on_conversational_messages() {
        let mut user = json!({"role": "user", "content": "inspect"});
        assert!(mark_turn_message(&mut user, "chain-1"));
        assert_eq!(
            turn_message_provenance(&user)
                .unwrap()
                .unwrap()
                .turn_chain_id,
            "chain-1"
        );
        clear_turn_message_provenance(&mut user);
        assert_eq!(turn_message_provenance(&user).unwrap(), None);

        let mut system = json!({"role": "system", "content": "runtime"});
        assert!(!mark_turn_message(&mut system, "chain-1"));
    }

    #[test]
    fn malformed_producer_metadata_fails_closed() {
        let message = json!({
            "role": "user",
            "content": "inspect",
            TURN_MESSAGE_PROVENANCE_FIELD: {
                "schema_version": 1,
                "turn_chain_id": ""
            }
        });
        assert!(matches!(
            turn_message_provenance(&message),
            Err(TurnMessageProvenanceError::EmptyTurnChainId)
        ));
    }
}
