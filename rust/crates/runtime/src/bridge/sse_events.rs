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

pub(super) fn find_sse_frame_end(buffer: &[u8]) -> Option<usize> {
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

pub(super) fn parse_sse_json_frame(frame: &[u8]) -> Option<serde_json::Value> {
    std::str::from_utf8(frame)
        .ok()
        .and_then(|frame| frame.strip_prefix("data: "))
        .and_then(|frame| frame.strip_suffix("\n\n"))
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
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
    build_turn_complete_event(
        has_tool_calls,
        stall_detected,
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
