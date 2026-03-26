use super::*;

pub(super) fn build_token_usage_from_usage_event(
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

pub(super) fn render_sse_json(event: serde_json::Value) -> Vec<u8> {
    format!("data: {}\n\n", serde_json::to_string(&event).unwrap()).into_bytes()
}

pub(super) fn parse_bridge_state_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    parse_sse_json_frame(frame)
        .and_then(|payload| payload.as_object().cloned())
        .filter(|payload| {
            payload.get("type").and_then(serde_json::Value::as_str) == Some("bridge_state")
        })
        .map(|mut payload| {
            payload.remove("type");
            payload.remove("session_id");
            payload
        })
}

pub fn parse_sse_json_frame(frame: &[u8]) -> Option<serde_json::Value> {
    // Delegate to the resilient parser which attempts recovery on malformed frames.
    parse_sse_json_frame_resilient(frame).ok()
}

pub(super) fn is_turn_complete_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "turn_complete")
    }) == Some(true)
}

pub(super) fn is_session_info_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "session_info")
    }) == Some(true)
}

pub(super) fn is_warning_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "warning")
    }) == Some(true)
}

pub(super) fn is_explain_frame(frame: &[u8]) -> bool {
    parse_sse_json_frame(frame).and_then(|payload| {
        payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(|event_type| event_type == "explain")
    }) == Some(true)
}

pub(super) fn build_usage_event_from_frame(
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

pub(super) fn build_text_delta_event_from_frame(
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

pub(super) fn build_reasoning_delta_event_from_frame(
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

pub(super) fn build_tool_call_start_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("tool_call_start") {
        return None;
    }
    Some(serde_json::Map::from_iter([
        (
            "type".to_string(),
            serde_json::Value::String("tool_call_start".to_string()),
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
    ]))
}

pub(super) fn build_tool_call_event_from_frame(
    frame: &[u8],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let payload = parse_sse_json_frame(frame)?;
    let tool_call = payload.as_object()?;
    if tool_call.get("type").and_then(serde_json::Value::as_str) != Some("tool_call") {
        return None;
    }
    Some(build_edge_tool_call_event(tool_call))
}

pub(super) fn build_error_event_from_frame(
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

pub(super) fn build_cloud_loop_progress_event_from_frame(
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

pub(super) fn build_cloud_tool_result_event_from_frame(
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

pub(super) fn build_tool_result_quality_event_from_frame(
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

pub(super) fn build_turn_complete_event_from_bridge_state(
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    trusted_execution_state: Option<&serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let tool_sigs = bridge_state_tool_signatures(bridge_state).unwrap_or_default();
    let has_tool_calls = !tool_sigs.is_empty();
    let stall_detected = detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW);
    let divergence_status = detect_divergence(&tool_sigs);
    build_turn_complete_event(
        has_tool_calls,
        stall_detected,
        &divergence_status,
        trusted_execution_state
            .and_then(serde_json::Value::as_object)
            .map(normalize_execution_state)
            .map(serde_json::Value::Object),
    )
}

pub(super) fn bridge_state_tool_signatures(
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
    if s.len() <= 200 {
        s.to_string()
    } else {
        format!("{}…({} bytes)", &s[..200], s.len())
    }
}

/// Like [`parse_sse_json_frame`] but with recovery for malformed frames.
///
/// Recovery strategy:
/// 1. Strip `data: ` prefix and `\n\n` suffix as normal.
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

    let payload = text
        .strip_prefix("data: ")
        .ok_or(SseParseError::MissingDataPrefix)?;
    let payload = payload.strip_suffix("\n\n").unwrap_or(payload);

    // Fast path: valid JSON
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
        return Ok(value);
    }

    // Slow path: try recovery
    let initial_err_msg = serde_json::from_str::<serde_json::Value>(payload)
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
        raw: payload.to_string(),
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
}
