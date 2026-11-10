//! Decode one blank-line-delimited SSE event into its JSON payload.

use serde_json::Value;

/// Parsed JSON payload or stream terminator from one SSE event.
#[derive(Debug, Default)]
pub struct SseJsonDrain {
    pub events: Vec<Value>,
    pub stream_finished: bool,
}

/// Decode a complete SSE event. JSON is parsed after all `data` fields have
/// been joined, so a multi-line event remains one logical message.
pub fn validated_sse_event(block: &str) -> Result<SseJsonDrain, String> {
    let Some(data) = astra_core::sse_blocks::event_data(block) else {
        return Ok(SseJsonDrain::default());
    };
    if data.trim().is_empty() {
        return Ok(SseJsonDrain::default());
    }
    if data == "[DONE]" {
        return Ok(SseJsonDrain {
            events: Vec::new(),
            stream_finished: true,
        });
    }
    let value = serde_json::from_str(&data).map_err(|error| {
        format!(
            "invalid JSON in SSE event ({} data bytes): {error}",
            data.len()
        )
    })?;
    Ok(SseJsonDrain {
        events: vec![value],
        stream_finished: false,
    })
}

/// Decode an event on observation-only paths that tolerate invalid payloads.
pub fn json_events_from_sse_event_block(block: &str) -> SseJsonDrain {
    validated_sse_event(block).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn joined_data_is_one_event() {
        let event = validated_sse_event("event: message\ndata:{\"a\":1,\ndata: \"b\":2}")
            .expect("valid joined JSON");
        assert_eq!(event.events, vec![json!({"a": 1, "b": 2})]);
        assert!(!event.stream_finished);
    }

    #[test]
    fn done_is_the_whole_event() {
        assert!(validated_sse_event("data:[DONE]").unwrap().stream_finished);
        assert!(validated_sse_event("data: [DONE]\ndata: {}").is_err());
        assert!(validated_sse_event("data: {\"a\":1}\ndata: {\"b\":2}").is_err());
    }

    #[test]
    fn metadata_has_no_data_and_invalid_data_fails() {
        assert!(
            validated_sse_event(": ping\nevent: heartbeat")
                .unwrap()
                .events
                .is_empty()
        );
        assert!(validated_sse_event("data:").unwrap().events.is_empty());
        assert!(validated_sse_event("data: not-json").is_err());
    }
}
