use std::collections::BTreeSet;
use crate::complete::build_turn_complete_event;
use crate::stream_events::build_edge_tool_call_event;
use crate::stall::{detect_server_stall, detect_divergence, SERVER_STALL_WINDOW};
use crate::execution_state::normalize_execution_state;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// Stream event protocol version encoding follows the step-protocol convention:
/// `major * 1000 + minor`. Keep this independent from checkpoint protocol changes.
pub const STREAM_EVENT_PROTOCOL_VERSION_MAJOR: u32 = 1;
pub const STREAM_EVENT_PROTOCOL_VERSION_MINOR: u32 = 0;
pub const STREAM_EVENT_PROTOCOL_VERSION: u32 =
    STREAM_EVENT_PROTOCOL_VERSION_MAJOR * 1000 + STREAM_EVENT_PROTOCOL_VERSION_MINOR;
const STREAM_EVENT_ID_PREFIX: &str = "mo-stream-v1.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEventCursor {
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_chain_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_query_event_id: Option<String>,
}

impl StreamEventCursor {
    pub fn from_event(event: &serde_json::Value) -> Option<Self> {
        if let Some(event_id) = event.get("event_id").and_then(serde_json::Value::as_str)
            && let Some(cursor) = parse_stream_event_id(event_id)
        {
            return Some(cursor);
        }
        Some(Self {
            sequence: event.get("sequence").and_then(serde_json::Value::as_u64)?,
            session_id: event
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            turn_chain_id: event
                .get("turn_chain_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            user_query_event_id: event
                .get("user_query_event_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
        })
    }

    pub fn has_replay_scope(&self) -> bool {
        self.session_id.is_some()
            || self.turn_chain_id.is_some()
            || self.user_query_event_id.is_some()
    }

    pub fn matches_scope(&self, other: &Self) -> bool {
        self.has_replay_scope()
            && scope_field_matches(self.session_id.as_deref(), other.session_id.as_deref())
            && scope_field_matches(
                self.turn_chain_id.as_deref(),
                other.turn_chain_id.as_deref(),
            )
            && scope_field_matches(
                self.user_query_event_id.as_deref(),
                other.user_query_event_id.as_deref(),
            )
    }
}

fn scope_field_matches(expected: Option<&str>, actual: Option<&str>) -> bool {
    match expected {
        Some(expected) => actual == Some(expected),
        None => true,
    }
}

fn scoped_cursor_value(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn render_stream_event_id(
    sequence: u64,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) -> String {
    let cursor = StreamEventCursor {
        sequence,
        session_id: scoped_cursor_value(session_id),
        turn_chain_id: scoped_cursor_value(turn_chain_id),
        user_query_event_id: scoped_cursor_value(user_query_event_id),
    };
    let encoded = serde_json::to_vec(&cursor)
        .ok()
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .unwrap_or_else(|| URL_SAFE_NO_PAD.encode(format!("{{\"sequence\":{sequence}}}")));
    format!("{STREAM_EVENT_ID_PREFIX}{encoded}")
}

pub fn parse_stream_event_id(event_id: &str) -> Option<StreamEventCursor> {
    let encoded = event_id.strip_prefix(STREAM_EVENT_ID_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let cursor = serde_json::from_slice::<StreamEventCursor>(&bytes).ok()?;
    (cursor.sequence > 0).then_some(cursor)
}

pub fn add_stream_event_metadata(
    event: &mut serde_json::Value,
    sequence: u64,
    session_id: Option<&str>,
    turn_chain_id: Option<&str>,
    user_query_event_id: Option<&str>,
) {
    let Some(obj) = event.as_object_mut() else {
        return;
    };
    obj.entry("sequence".to_string())
        .or_insert_with(|| serde_json::Value::from(sequence));
    obj.entry("event_id".to_string()).or_insert_with(|| {
        serde_json::Value::String(render_stream_event_id(
            sequence,
            session_id,
            turn_chain_id,
            user_query_event_id,
        ))
    });
    if let Some(turn_chain_id) = turn_chain_id.filter(|value| !value.is_empty()) {
        obj.entry("turn_chain_id".to_string())
            .or_insert_with(|| serde_json::Value::String(turn_chain_id.to_string()));
    }
    if let Some(user_query_event_id) = user_query_event_id.filter(|value| !value.is_empty()) {
        obj.entry("user_query_event_id".to_string())
            .or_insert_with(|| serde_json::Value::String(user_query_event_id.to_string()));
    }
}

pub fn build_token_usage_from_usage_event(
    usage_event: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let prompt = usage_event.get("prompt_tokens")?.as_i64()?;
    let completion = usage_event.get("completion_tokens")?.as_i64()?;
    Some(serde_json::json!({
        "prompt": prompt,
        "completion": completion,
        "total": prompt + completion,
    }))
}

pub fn find_sse_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|window| window == b"\n\n")
}

pub fn render_sse_json(event: serde_json::Value) -> Vec<u8> {
    let mut event = event;
    if let Some(obj) = event.as_object_mut() {
        obj.entry("protocol_version".to_string())
            .or_insert_with(|| serde_json::Value::from(STREAM_EVENT_PROTOCOL_VERSION));
    }
    let event_id = event
        .as_object()
        .and_then(|obj| obj.get("event_id"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    match serde_json::to_string(&event) {
        Ok(json) => {
            let mut frame = Vec::new();
            if let Some(event_id) = event_id {
                frame.extend_from_slice(format!("id: {event_id}\n").as_bytes());
            }
            frame.extend_from_slice(format!("data: {json}\n\n").as_bytes());
            frame
        }
        Err(_) => b"data: {\"type\":\"error\",\"message\":\"serialization failed\"}\n\n".to_vec(),
    }
}

pub fn parse_bridge_state_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    parse_sse_json_frame(frame)
        .and_then(|payload| payload.as_object().cloned())
        .filter(|payload| {
            payload.get("type").and_then(serde_json::Value::as_str) == Some("bridge_state")
        })
        .map(|mut payload| {
            payload.remove("type");
            payload.remove("protocol_version");
            payload.remove("session_id");
            payload
        })
}

pub fn parse_sse_json_frame(frame: &[u8]) -> Option<serde_json::Value> {
    // Delegate to the resilient parser which attempts recovery on malformed frames.
    parse_sse_json_frame_resilient(frame).ok()
}

pub fn is_turn_complete_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "turn_complete")
    }) == Some(true)
}

pub fn is_session_info_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "session_info")
    }) == Some(true)
}

pub fn is_warning_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "warning")
    }) == Some(true)
}

pub fn is_explain_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "explain")
    }) == Some(true)
}

pub fn build_usage_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("usage") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("usage".to_string()),
        ),
        (
            "prompt_tokens".to_string(),
            serde_json::Value::from(
                payload
                    .get("prompt_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
        (
            "completion_tokens".to_string(),
            serde_json::Value::from(
                payload
                    .get("completion_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
        (
            "cache_read_tokens".to_string(),
            serde_json::Value::from(
                payload
                    .get("cache_read_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
    ]))
}

pub fn build_text_delta_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("text_delta") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("text_delta".to_string()),
        ),
        (
            "content".to_string(),
            serde_json::Value::String(
                payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        ),
    ]))
}

pub fn build_reasoning_delta_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("reasoning_delta") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("reasoning_delta".to_string()),
        ),
        (
            "content".to_string(),
            serde_json::Value::String(
                payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        ),
    ]))
}

pub fn build_tool_call_start_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("tool_call_start") {
        return None;
    }
    let payload = payload.as_object()?;
    Some(serde_json::Map::from_iter(
        payload
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "type" | "tool" | "call_id" | "arguments"))
            .map(|(key, value)| (key.clone(), value.clone())),
    ))
}

pub fn build_tool_call_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    let tool_call = payload.as_object()?;
    if tool_call.get("type").and_then(serde_json::Value::as_str) != Some("tool_call") {
        return None;
    }
    Some(build_edge_tool_call_event(tool_call))
}

pub fn build_error_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    let error = payload.as_object()?;
    if error.get("type").and_then(serde_json::Value::as_str) != Some("error") {
        return None;
    }
    Some(serde_json::Map::from_iter(
        error
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "type" | "message" | "code" | "retryable" | "retry_after_ms"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone())),
    ))
}

pub fn build_cloud_loop_progress_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("cloud_loop_progress") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("cloud_loop_progress".to_string()),
        ),
        (
            "loop".to_string(),
            serde_json::Value::from(
                payload
                    .get("loop")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
        (
            "cloud_skills".to_string(),
            serde_json::Value::from(
                payload
                    .get("cloud_skills")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
        (
            "edge_skills".to_string(),
            serde_json::Value::from(
                payload
                    .get("edge_skills")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            ),
        ),
    ]))
}

pub fn build_cloud_tool_result_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("cloud_tool_result") {
        return None;
    }
    let mut event = serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("cloud_tool_result".to_string()),
        ),
        (
            "name".to_string(),
            serde_json::Value::String(
                payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        ),
        (
            "result".to_string(),
            serde_json::Value::String(
                payload
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        ),
    ]);
    if let Some(blocked) = payload.get("blocked").and_then(serde_json::Value::as_bool) {
        event.insert("blocked".to_string(), serde_json::Value::Bool(blocked));
    }
    Some(event)
}

pub fn build_tool_result_quality_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    let assessment = payload.as_object()?;
    if assessment.get("type").and_then(serde_json::Value::as_str) != Some("tool_result_quality") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("tool_result_quality".to_string()),
        ),
        (
            "tool_name".to_string(),
            assessment
                .get("tool_name")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "grade".to_string(),
            assessment
                .get("grade")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "score".to_string(),
            assessment
                .get("score")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "signals".to_string(),
            assessment
                .get("signals")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        ),
    ]))
}

pub fn build_turn_complete_event_from_bridge_state(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    trusted_execution_state: Option<&serde_json::Value>,
    latest_user_message: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let tool_sigs = bridge_state_tool_signatures(bridge_state).unwrap_or_default();
    let has_tool_calls = !tool_sigs.is_empty();
    let stall_detected = detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW);
    let divergence_status = detect_divergence(&tool_sigs);
    let mut event = build_turn_complete_event(
        has_tool_calls,
        stall_detected,
        &divergence_status,
        trusted_execution_state
            .and_then(serde_json::Value::as_object)
            .map(normalize_execution_state)
            .map(serde_json::Value::Object),
    );
    if let Some(suggestion) = latest_user_message
        .and_then(|user_message| build_followup_suggestion(bridge_state, user_message))
    {
        event.insert(
            "followup_suggestion".to_string(),
            serde_json::Value::String(suggestion.text),
        );
    }
    event
}

pub fn bridge_state_tool_signatures(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
) -> Option<Vec<BTreeSet<String>>> {
    bridge_state
        .get("tool_sigs")
        .and_then(serde_json::Value::as_array)
        .map(|tool_sigs| {
            tool_sigs
                .iter()
                .map(|sig| {
                    sig.as_array().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(ToString::to_string))
                            .collect::<Option<BTreeSet<_>>>()
                    })
                })
                .collect::<Option<Vec<_>>>()
        })?
}

fn build_followup_suggestion(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    user_message: &str,
) -> Option<crate::followup_suggestion::FollowupSuggestion> {
    let assistant_turn = latest_assistant_turn(bridge_state)?;
    crate::followup_suggestion::suggest_followup(
        user_message,
        &assistant_turn.0,
        &assistant_turn.1,
    )
}

fn latest_assistant_turn(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
) -> Option<(String, Vec<String>)> {
    let assistant = bridge_state
        .get("history")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .rev()
        .find(|entry| entry.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))?;
    let content = assistant
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tool_names = assistant
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .map(|tool_calls| {
            tool_calls
                .iter()
                .filter_map(extract_tool_name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some((content, tool_names))
}

fn extract_tool_name(tool_call: &serde_json::Value) -> Option<String> {
    tool_call
        .get("function")
        .and_then(serde_json::Value::as_object)
        .and_then(|function| function.get("name"))
        .or_else(|| tool_call.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

// ── SSE frame resilience ─────────────────────────────────────────────────────

/// Structured error for resilient SSE frame parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseParseError {
    EmptyFrame,
    MissingDataPrefix,
    InvalidJson { raw: String, error: String },
    Unrecoverable { raw: String },
}

impl std::fmt::Display for SseParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyFrame => write!(f, "empty SSE frame"),
            Self::MissingDataPrefix => write!(f, "SSE frame missing 'data: ' prefix"),
            Self::InvalidJson { error, .. } => write!(f, "invalid JSON in SSE frame: {error}"),
            Self::Unrecoverable { raw } => {
                write!(f, "unrecoverable SSE frame: {}", truncate_for_debug(raw))
            }
        }
    }
}

fn truncate_for_debug(s: &str) -> String {
    use astra_text_utils::str_preview::truncate_str;

    let preview = truncate_str(s, 200);
    if preview == s {
        preview
    } else {
        format!("{preview} ({} bytes)", s.len())
    }
}

/// Like [`parse_sse_json_frame`] but with recovery for malformed frames.
///
/// Recovery strategy:
/// 1. Ignore SSE metadata lines and join all `data:` lines.
/// 2. Try standard JSON parse.
/// 3. On failure: trim trailing whitespace/garbage, find the last `}`, take
///    the substring up to and including it, and re-parse.
pub fn parse_sse_json_frame_resilient(frame: &[u8]) -> Result<serde_json::Value, SseParseError> {
    let text = std::str::from_utf8(frame).map_err(|_| SseParseError::Unrecoverable {
        raw: String::from_utf8_lossy(frame).into_owned(),
    })?;

    if text.trim().is_empty() {
        return Err(SseParseError::EmptyFrame);
    }

    let payload = text.strip_suffix("\n\n").unwrap_or(text);
    let data_lines = payload
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        })
        .collect::<Vec<_>>();
    if data_lines.is_empty() {
        return Err(SseParseError::MissingDataPrefix);
    }
    let payload = data_lines.join("\n");

    // Fast path: valid JSON
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) {
        return Ok(value);
    }

    // Slow path: try recovery
    let initial_err_msg = serde_json::from_str::<serde_json::Value>(&payload)
        .unwrap_err()
        .to_string();
    let trimmed = payload.trim();
    if let Some(last_brace) = trimmed.rfind('}') {
        let candidate = &trimmed[..=last_brace];
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            return Ok(value);
        }
    }

    // Could not recover
    Err(SseParseError::InvalidJson {
        raw: payload,
        error: initial_err_msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resilient_parse_valid_frame() {
        let frame = b"data: {\"type\":\"text_delta\",\"content\":\"hi\"}\n\n";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["type"], "text_delta");
        assert_eq!(val["content"], "hi");
    }

    #[test]
    fn resilient_parse_without_trailing_newlines() {
        let frame = b"data: {\"type\":\"ping\"}";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["type"], "ping");
    }

    #[test]
    fn resilient_parse_with_sse_id_metadata() {
        let frame = b"id: mo-stream-v1.test\nevent: message\ndata: {\"type\":\"ping\"}\n\n";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["type"], "ping");
    }

    #[test]
    fn resilient_parse_recovers_trailing_garbage() {
        let frame = b"data: {\"type\":\"text_delta\",\"content\":\"ok\"}garbage\n\n";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["type"], "text_delta");
        assert_eq!(val["content"], "ok");
    }

    #[test]
    fn resilient_parse_recovers_trailing_whitespace_garbage() {
        let frame = b"data: {\"ok\":true}  \x00\x00\n\n";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["ok"], true);
    }

    #[test]
    fn resilient_parse_empty_frame() {
        let err = parse_sse_json_frame_resilient(b"").unwrap_err();
        assert_eq!(err, SseParseError::EmptyFrame);
    }

    #[test]
    fn resilient_parse_whitespace_only() {
        let err = parse_sse_json_frame_resilient(b"   \n\n").unwrap_err();
        assert_eq!(err, SseParseError::EmptyFrame);
    }

    #[test]
    fn resilient_parse_missing_data_prefix() {
        let err = parse_sse_json_frame_resilient(b"event: {\"a\":1}\n\n").unwrap_err();
        assert_eq!(err, SseParseError::MissingDataPrefix);
    }

    #[test]
    fn resilient_parse_unrecoverable_json() {
        let frame = b"data: not json at all\n\n";
        let err = parse_sse_json_frame_resilient(frame).unwrap_err();
        assert!(matches!(err, SseParseError::InvalidJson { .. }));
    }

    #[test]
    fn resilient_parse_invalid_utf8() {
        let frame: &[u8] = &[0xFF, 0xFE, 0xFD];
        let err = parse_sse_json_frame_resilient(frame).unwrap_err();
        assert!(matches!(err, SseParseError::Unrecoverable { .. }));
    }

    #[test]
    fn resilient_parse_nested_json() {
        let frame = b"data: {\"type\":\"tool_call\",\"args\":{\"path\":\"/a/b\"}}\n\n";
        let val = parse_sse_json_frame_resilient(frame).unwrap();
        assert_eq!(val["args"]["path"], "/a/b");
    }

    #[test]
    fn sse_parse_error_display() {
        assert_eq!(format!("{}", SseParseError::EmptyFrame), "empty SSE frame");
        assert_eq!(
            format!("{}", SseParseError::MissingDataPrefix),
            "SSE frame missing 'data: ' prefix"
        );
    }

    // ── Delegation tests: parse_sse_json_frame now uses resilient recovery ──

    #[test]
    fn standard_parse_recovers_trailing_garbage() {
        // Before wiring: this returned None (silently dropped).
        // After wiring: recovers via rfind('}') in resilient parser.
        let frame = b"data: {\"type\":\"text_delta\",\"content\":\"ok\"}garbage\n\n";
        let val = parse_sse_json_frame(frame);
        assert!(
            val.is_some(),
            "standard parse should now recover trailing garbage"
        );
        assert_eq!(val.unwrap()["content"], "ok");
    }

    #[test]
    fn standard_parse_still_works_for_valid_frames() {
        let frame = b"data: {\"type\":\"done\"}\n\n";
        let val = parse_sse_json_frame(frame).unwrap();
        assert_eq!(val["type"], "done");
    }

    #[test]
    fn standard_parse_returns_none_for_empty() {
        assert!(parse_sse_json_frame(b"").is_none());
    }

    #[test]
    fn standard_parse_returns_none_for_invalid_utf8() {
        assert!(parse_sse_json_frame(&[0xFF, 0xFE]).is_none());
    }

    #[test]
    fn render_sse_json_injects_protocol_version() {
        let frame = render_sse_json(serde_json::json!({
            "type": "text_delta",
            "content": "hello",
        }));
        let parsed = parse_sse_json_frame(&frame).expect("rendered frame should parse");
        assert_eq!(
            parsed
                .get("protocol_version")
                .and_then(serde_json::Value::as_u64),
            Some(STREAM_EVENT_PROTOCOL_VERSION as u64)
        );
    }

    #[test]
    fn render_sse_json_emits_id_line_when_event_id_present() {
        let frame = render_sse_json(serde_json::json!({
            "type": "text_delta",
            "content": "hello",
            "event_id": "mo-stream-v1.test",
        }));
        let text = String::from_utf8(frame.clone()).expect("frame should be utf8");
        assert!(text.starts_with("id: mo-stream-v1.test\n"));
        let parsed = parse_sse_json_frame(&frame).expect("rendered frame should parse");
        assert_eq!(parsed["event_id"], "mo-stream-v1.test");
    }

    #[test]
    fn stream_event_id_roundtrips() {
        let event_id = render_stream_event_id(7, Some("sess-1"), Some("turn-1"), Some("query-1"));
        let cursor = parse_stream_event_id(&event_id).expect("event id should decode");
        assert_eq!(cursor.sequence, 7);
        assert_eq!(cursor.session_id.as_deref(), Some("sess-1"));
        assert_eq!(cursor.turn_chain_id.as_deref(), Some("turn-1"));
        assert_eq!(cursor.user_query_event_id.as_deref(), Some("query-1"));
        assert!(cursor.has_replay_scope());
    }

    #[test]
    fn render_sse_json_preserves_existing_protocol_version() {
        let frame = render_sse_json(serde_json::json!({
            "type": "text_delta",
            "content": "hello",
            "protocol_version": 2001,
        }));
        let parsed = parse_sse_json_frame(&frame).expect("rendered frame should parse");
        assert_eq!(
            parsed
                .get("protocol_version")
                .and_then(serde_json::Value::as_u64),
            Some(2001)
        );
    }

    #[test]
    fn add_stream_event_metadata_inserts_sequence_and_correlation_fields() {
        let mut event = serde_json::json!({
            "type": "warning",
            "message": "watch out",
        });
        add_stream_event_metadata(
            &mut event,
            7,
            Some("sess-1"),
            Some("turn-1"),
            Some("query-1"),
        );
        assert_eq!(event["sequence"], 7);
        assert_eq!(event["turn_chain_id"], "turn-1");
        assert_eq!(event["user_query_event_id"], "query-1");
        assert!(event["event_id"].as_str().is_some());
    }

    #[test]
    fn parse_bridge_state_frame_strips_protocol_envelope() {
        let frame = b"data: {\"type\":\"bridge_state\",\"session_id\":\"s1\",\"protocol_version\":1000,\"tail_full_text\":\"hello\"}\n\n";
        let parsed = parse_bridge_state_frame(frame).expect("bridge_state frame");
        assert_eq!(
            parsed
                .get("tail_full_text")
                .and_then(serde_json::Value::as_str),
            Some("hello")
        );
        assert!(!parsed.contains_key("type"));
        assert!(!parsed.contains_key("session_id"));
        assert!(!parsed.contains_key("protocol_version"));
    }

    #[test]
    fn turn_complete_event_includes_followup_suggestion_when_available() {
        let bridge_state = serde_json::json!({
            "tool_sigs": [["str_replace:{}"], ["run_build_test:{}"]],
            "history": [{
                "role": "assistant",
                "content": "Patched and verified.",
                "tool_calls": [
                    {"function": {"name": "str_replace"}},
                    {"function": {"name": "run_build_test"}}
                ]
            }]
        });
        let event = build_turn_complete_event_from_bridge_state(
            bridge_state.as_object().expect("object"),
            None,
            Some("Fix the bug"),
        );
        assert_eq!(
            event.get("followup_suggestion"),
            Some(&serde_json::json!("commit this"))
        );
    }
}
