//! Immutable C1 identities shared by the canonical writer and replay readers.
//! Reasoning enrichment is presentation; all other payload fields remain identity.

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn transcript_payload_identity(
    role: &str,
    payload: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let mut value: Value = serde_json::from_str(payload)
        .map_err(|error| format!("invalid transcript payload: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "transcript payload must be an object".to_owned())?;
    if role == "assistant" {
        object.remove("reasoning");
        object.remove("reasoning_status");
    }
    if object.is_empty() {
        return Ok(None);
    }
    Ok(Some(astra_core::canonical_json_string(&value)))
}

pub fn transcript_content_hash(role: &str, content: &str, payload_json: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update(content.as_bytes());
    hasher.update([0]);
    if let Some(payload_json) = payload_json {
        hasher.update(payload_json.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
