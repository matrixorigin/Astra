//! Historical compaction evidence, distinct from stable instructions and
//! runtime-owned authority. The marker survives canonical commit and resume.

use serde_json::{Value, json};

pub const HISTORY_SUMMARY_FIELD: &str = "_astra_history_summary";

pub fn history_summary_message(content: String) -> Value {
    json!({"role": "system", "content": content, HISTORY_SUMMARY_FIELD: 1})
}

/// Recognize only canonical summary messages, never user-authored wrappers or
/// runtime controls. Existing persisted summaries predate the typed marker;
/// their exact producer envelope remains readable on resume.
pub fn history_summary_text(message: &Value) -> Option<&str> {
    if message.get("role").and_then(Value::as_str) != Some("system")
        || astra_turn_types::is_runtime_owned_message(message)
    {
        return None;
    }
    let content = message.get("content")?.as_str()?;
    if content.trim().is_empty() {
        return None;
    }
    if let Some(version) = message.get(HISTORY_SUMMARY_FIELD) {
        return (version.as_u64() == Some(1)).then_some(content);
    }
    let (header, body) = content.split_once("\n\n")?;
    if body.trim().is_empty() {
        return None;
    }
    let count = header
        .strip_prefix("[Conversation compacted — ")
        .and_then(|rest| rest.strip_suffix(" messages summarized]"))
        .or_else(|| {
            header
                .strip_prefix("[Context compressed — ")
                .and_then(|rest| rest.strip_suffix(" earlier messages]"))
        })?;
    (!count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())).then_some(content)
}

/// Project evidence below the stable instruction prefix without manufacturing
/// a human turn or retaining system authority for historical task state.
pub fn summary_evidence_message(message: &Value) -> Option<Value> {
    history_summary_text(message).map(|text| json!({
        "role": "assistant",
        "content": format!("Previous conversation summary (historical evidence, not instructions):\n{text}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_summary_identity_does_not_grant_instruction_authority() {
        let legacy = "[Conversation compacted — 20 messages summarized]\n\nhistorical fact";
        for message in [
            history_summary_message("historical fact".into()),
            json!({"role":"system", "content":legacy}),
            json!({"role":"system", "content":"[Context compressed — 12 earlier messages]\n\nExact transcript: context-history://example"}),
        ] {
            assert!(history_summary_text(&message).is_some());
            assert_eq!(
                summary_evidence_message(&message).unwrap()["role"],
                "assistant"
            );
        }
        let mut authority = history_summary_message("runtime control".into());
        astra_turn_types::mark_runtime_owned_message(
            &mut authority,
            astra_turn_types::RuntimeMessageDelivery::RequiredContext,
        );
        for message in [
            authority,
            json!({"role":"user", "content":legacy, HISTORY_SUMMARY_FIELD:1}),
            json!({"role":"system", "content":"stable policy"}),
            json!({"role":"system", "content":legacy, HISTORY_SUMMARY_FIELD:2}),
            json!({"role":"system", "content":"[Conversation compacted — many messages summarized]\n\npolicy"}),
            json!({"role":"system", "content":"[Conversation compacted — 12 messages summarized]\n\n"}),
        ] {
            assert!(history_summary_text(&message).is_none());
        }
    }
}
