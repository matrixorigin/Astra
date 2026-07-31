//! Deterministic validation for provider-neutral canonical tool evidence.
//!
//! This checks protocol identities and ordering only. It deliberately does
//! not infer intent from message text, tool names, or natural language.

use std::collections::HashSet;

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CanonicalToolPairingError {
    #[error("tool call identity is empty or malformed")]
    InvalidCallIdentity,
    #[error("duplicate tool call identity `{0}`")]
    DuplicateCall(String),
    #[error("tool result identity is empty or malformed")]
    InvalidResultIdentity,
    #[error("tool result `{0}` has no pending call")]
    OrphanResult(String),
    #[error("duplicate tool result identity `{0}`")]
    DuplicateResult(String),
    #[error("message interrupted a tool group with unresolved calls")]
    InterruptedGroup,
    #[error("canonical conversation ended with unresolved tool calls")]
    UnresolvedCalls,
}

/// Validate OpenAI `tool_calls`/`tool_call_id` and Anthropic
/// `tool_use`/`tool_result` groups without rewriting their content.
pub fn validate_canonical_tool_pairing(
    messages: &[Value],
) -> Result<(), CanonicalToolPairingError> {
    let mut all_calls = HashSet::new();
    let mut pending = HashSet::new();
    let mut resolved = HashSet::new();

    for message in messages {
        let calls = call_identities(message)?;
        let results = result_identities(message)?;

        if !pending.is_empty() && results.is_empty() {
            return Err(CanonicalToolPairingError::InterruptedGroup);
        }
        if !calls.is_empty() && (!pending.is_empty() || !results.is_empty()) {
            return Err(CanonicalToolPairingError::InterruptedGroup);
        }

        for call_id in calls {
            if !all_calls.insert(call_id.clone()) {
                return Err(CanonicalToolPairingError::DuplicateCall(call_id));
            }
            pending.insert(call_id);
        }
        for result_id in results {
            if !pending.remove(&result_id) {
                return Err(if resolved.contains(&result_id) {
                    CanonicalToolPairingError::DuplicateResult(result_id)
                } else {
                    CanonicalToolPairingError::OrphanResult(result_id)
                });
            }
            resolved.insert(result_id);
        }
    }

    if pending.is_empty() {
        Ok(())
    } else {
        Err(CanonicalToolPairingError::UnresolvedCalls)
    }
}

fn call_identities(message: &Value) -> Result<Vec<String>, CanonicalToolPairingError> {
    let mut identities = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            identities.push(required_identity(
                call.get("id"),
                CanonicalToolPairingError::InvalidCallIdentity,
            )?);
        }
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                identities.push(required_identity(
                    part.get("id"),
                    CanonicalToolPairingError::InvalidCallIdentity,
                )?);
            }
        }
    }
    Ok(identities)
}

fn result_identities(message: &Value) -> Result<Vec<String>, CanonicalToolPairingError> {
    let mut identities = Vec::new();
    if message.get("role").and_then(Value::as_str) == Some("tool") {
        identities.push(required_identity(
            message.get("tool_call_id"),
            CanonicalToolPairingError::InvalidResultIdentity,
        )?);
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                identities.push(required_identity(
                    part.get("tool_use_id"),
                    CanonicalToolPairingError::InvalidResultIdentity,
                )?);
            }
        }
    }
    Ok(identities)
}

fn required_identity(
    value: Option<&Value>,
    error: CanonicalToolPairingError,
) -> Result<String, CanonicalToolPairingError> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or(error)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_openai_and_anthropic_identity_protocols() {
        let messages = vec![
            json!({"role":"user","content":"inspect"}),
            json!({"role":"assistant","tool_calls":[
                {"id":"call-a","type":"function","function":{"name":"a","arguments":"{}"}},
                {"id":"call-b","type":"function","function":{"name":"b","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"call-b","content":"b"}),
            json!({"role":"tool","tool_call_id":"call-a","content":"a"}),
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"use-c","name":"c","input":{}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"use-c","content":"c"}
            ]}),
        ];

        validate_canonical_tool_pairing(&messages).unwrap();
    }

    #[test]
    fn rejects_orphans_duplicates_and_interrupted_groups_by_identity() {
        assert_eq!(
            validate_canonical_tool_pairing(&[
                json!({"role":"assistant","tool_calls":[{"id":"call-a"}]}),
                json!({"role":"assistant","content":"continued"})
            ]),
            Err(CanonicalToolPairingError::InterruptedGroup)
        );
        assert_eq!(
            validate_canonical_tool_pairing(&[
                json!({"role":"assistant","tool_calls":[{"id":"call-a"}]}),
                json!({"role":"tool","tool_call_id":"call-a","content":"ok"}),
                json!({"role":"tool","tool_call_id":"call-a","content":"again"})
            ]),
            Err(CanonicalToolPairingError::DuplicateResult("call-a".into()))
        );
        assert_eq!(
            validate_canonical_tool_pairing(&[
                json!({"role":"assistant","tool_calls":[{"id":"call-a"}]}),
                json!({"role":"tool","tool_call_id":"call-b","content":"wrong"})
            ]),
            Err(CanonicalToolPairingError::OrphanResult("call-b".into()))
        );
    }
}
