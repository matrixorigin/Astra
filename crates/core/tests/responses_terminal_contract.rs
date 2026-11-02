use astra_core::model_wire::responses::{
    CONTINUATION_KEY, decode_sse, request, validated_continuation,
};
use serde_json::{Value, json};
fn terminal(status: &str, output: Value) -> Value {
    json!({"type":if status == "completed" {"response.completed"} else {"response.incomplete"},"response":{"id":"resp-fixture","status":status,"incomplete_details":{"reason":"max_output_tokens"},"output":output,"usage":{"input_tokens":10,"output_tokens":512}}})
}
fn decode(
    event: Value,
) -> Result<
    astra_core::model_wire::responses::ResponseOutput,
    astra_core::model_wire::responses::ResponsesStreamError,
> {
    decode_sse(&format!("data: {event}\n\n"))
}
fn call(status: Option<&str>) -> Value {
    let mut item = json!({"type":"function_call","id":"fc-fixture","namespace":"astra","call_id":"call-fixture","name":"read_file","arguments":"{}"});
    if let Some(status) = status {
        item["status"] = json!(status);
    }
    item
}
#[test]
fn truncation_never_authorizes_functions_or_provider_replay() {
    for status in [
        None,
        Some("in_progress"),
        Some("incomplete"),
        Some("completed"),
    ] {
        let item = call(status);
        let end = terminal("incomplete", json!([item]));
        for done in [false, true] {
            let prefix = if done {
                format!(
                    "data: {}\n\n",
                    json!({"type":"response.output_item.done","output_index":0,"item":item})
                )
            } else {
                String::new()
            };
            let out = decode_sse(&format!("{prefix}data: {end}\n\n")).unwrap();
            assert!(out.tool_calls.is_empty());
            assert!(out.items.is_empty());
            assert_eq!(out.finish_reason.as_deref(), Some("length"));
            assert_eq!(out.response_id.as_deref(), Some("resp-fixture"));
            assert_eq!(out.usage["completion_tokens"], 512);
        }
    }
}
#[test]
fn empty_budget_terminal_preserves_length_usage_and_partial_text() {
    let end = terminal("incomplete", json!([]));
    for text in ["", "Partial answer"] {
        let prefix = format!(
            "data: {}\n\n",
            json!({"type":"response.output_text.delta","delta":text})
        );
        let out = decode_sse(&format!("{prefix}data: {end}\n\n")).unwrap();
        assert_eq!(out.text, text);
        assert_eq!(out.finish_reason.as_deref(), Some("length"));
        assert!(out.items.is_empty());
        assert!(out.tool_calls.is_empty());
        assert_eq!(out.usage["prompt_tokens"], 10);
    }
}
#[test]
fn completed_functions_require_complete_items_including_saved_replay() {
    let connection = "00000000-0000-4000-8000-000000000001";
    for (status, allowed) in [
        (None, true),
        (Some(Value::Null), true),
        (Some(json!("completed")), true),
        (Some(json!("in_progress")), false),
        (Some(json!("incomplete")), false),
        (Some(json!("unknown")), false),
        (Some(json!(123)), false),
    ] {
        for mut item in [
            call(None),
            json!({"type":"reasoning","id":"reasoning-fixture","summary":[],"encrypted_content":"ciphertext"}),
        ] {
            if let Some(status) = &status {
                item["status"] = status.clone();
            }
            for done_fallback in [false, true] {
                let end = terminal(
                    "completed",
                    if done_fallback {
                        json!([])
                    } else {
                        json!([item])
                    },
                );
                let prefix = if done_fallback {
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.output_item.done","output_index":0,"item":item})
                    )
                } else {
                    String::new()
                };
                let complete = decode_sse(&format!("{prefix}data: {end}\n\n"));
                assert_eq!(complete.is_ok(), allowed, "{item}");
                if let Ok(out) = complete {
                    assert_eq!(
                        out.tool_calls.len(),
                        usize::from(item["type"] == "function_call")
                    );
                }
            }
            let mut saved = json!({"role":"assistant","content":"","tool_calls":[{"id":"call-fixture","function":{"name":"read_file","arguments":"{}"}}]});
            saved[CONTINUATION_KEY] =
                json!({"connection_id":connection,"model":"model","items":[item]});
            assert_eq!(validated_continuation(&saved).is_some(), allowed);
            let wire = request("model", &[saved], None, connection, false, None);
            assert_eq!(wire.is_ok(), allowed);
            if let Ok(wire) = wire {
                assert!(
                    wire["input"][0]
                        .get("status")
                        .is_none_or(|value| value == "completed")
                );
            }
        }
    }
    for field in ["call_id", "name", "arguments"] {
        let mut item = call(Some("completed"));
        item[field] = Value::Null;
        assert!(
            decode(terminal("completed", json!([item]))).is_err(),
            "required {field}"
        );
    }
}
