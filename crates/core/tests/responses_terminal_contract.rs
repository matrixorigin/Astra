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
    let mut item = json!({"type":"function_call","id":"fc-fixture","call_id":"call-fixture","name":"read_file","arguments":"{}"});
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
            let is_call = item["type"] == "function_call";
            let mut saved = json!({"role":"assistant","content":""});
            if is_call {
                saved["tool_calls"] =
                    json!([{"id":"call-fixture","function":{"name":"read_file","arguments":"{}"}}]);
            }
            saved[CONTINUATION_KEY] =
                json!({"connection_id":connection,"model":"model","items":[item]});
            assert_eq!(validated_continuation(&saved).is_some(), allowed);
            let mut messages = vec![saved];
            if is_call {
                messages.push(json!({"role":"tool","tool_call_id":"call-fixture","content":"ok"}));
            }
            let wire = request("model", &messages, None, connection, false, None);
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

#[test]
fn completed_tool_rounds_validate_the_same_items_before_execution_and_replay() {
    let connection = "00000000-0000-4000-8000-000000000001";
    let valid = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"working"}]});
    let invalid_messages = [
        json!({"type":"message","content":[{"type":"output_text","text":"working"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"unknown","text":"working"}]}),
    ];
    assert!(decode(terminal("completed", json!([valid, call(None)]))).is_ok());
    for invalid in invalid_messages {
        for finished_items in [false, true] {
            let items = vec![invalid.clone(), call(None)];
            let mut stream = String::new();
            if finished_items {
                for (index, item) in items.iter().enumerate() {
                    stream.push_str(&format!(
                        "data: {}\n\n",
                        json!({"type":"response.output_item.done","output_index":index,"item":item})
                    ));
                }
            }
            let output = if finished_items {
                json!([])
            } else {
                json!(items)
            };
            stream.push_str(&format!("data: {}\n\n", terminal("completed", output)));
            assert!(decode_sse(&stream).is_err(), "invalid message: {invalid}");
        }
        let saved = json!({"role":"assistant", CONTINUATION_KEY:{
            "connection_id":connection,"model":"model","items":[invalid,call(None)]
        }});
        assert!(validated_continuation(&saved).is_none());
    }

    for (field, value) in [
        ("call_id", "x".repeat(513)),
        ("call_id", "call\ninvalid".to_owned()),
        ("name", "x".repeat(129)),
    ] {
        let mut invalid = call(None);
        invalid[field] = json!(value);
        assert!(decode(terminal("completed", json!([invalid]))).is_err());
        let saved = json!({"role":"assistant", CONTINUATION_KEY:{
            "connection_id":connection,"model":"model","items":[invalid]
        }});
        assert!(validated_continuation(&saved).is_none());
    }

    let mut items: Vec<_> = (0..4095)
        .map(|index| json!({"type":"message","id":format!("m{index}"),"role":"assistant","content":[{"type":"output_text","text":"x"}]}))
        .collect();
    items.push(call(None));
    let output = decode(terminal("completed", json!(items))).unwrap();
    assert_eq!(output.tool_calls.len(), 1);
    items.push(json!({"type":"reasoning","encrypted_content":"ciphertext"}));
    assert!(decode(terminal("completed", json!(items))).is_err());

    let mut stream = String::new();
    for index in 0..4097 {
        let item = if index == 0 {
            call(None)
        } else {
            json!({"type":"reasoning","encrypted_content":"ciphertext"})
        };
        stream.push_str(&format!(
            "data: {}\n\n",
            json!({
                "type":"response.output_item.done","output_index":index,"item":item
            })
        ));
    }
    stream.push_str(&format!("data: {}\n\n", terminal("completed", json!([]))));
    assert!(decode_sse(&stream).is_err());
}

fn message(id: &str, text: &str, status: &str) -> Value {
    json!({"type":"message","id":id,"role":"assistant","status":status,"content":[{"type":"output_text","text":text,"annotations":[]}]})
}
fn stream(full_terminal: bool) -> String {
    let first = message("msg-first", "First part. ", "completed");
    let second = message("msg-second", "Second part is unfinished", "incomplete");
    let mut events = vec![
        json!({"type":"response.output_text.delta","item_id":"msg-first","output_index":0,"content_index":0,"delta":"First part. "}),
        json!({"type":"response.output_item.done","output_index":0,"item":first}),
        json!({"type":"response.output_item.added","output_index":1,"item":message("msg-second","","in_progress")}),
        json!({"type":"response.output_text.delta","item_id":"msg-second","output_index":1,"content_index":0,"delta":"Second part is unfinished"}),
    ];
    events.push(json!({"type":"response.incomplete","response":{"id":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":if full_terminal {json!([first,second])} else {json!([])},"usage":{"input_tokens":10,"output_tokens":50}}}));
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}
#[test]
fn full_terminal_control_preserves_both_messages() {
    let out = decode_sse(&stream(true)).unwrap();
    assert_eq!(out.text, "First part. Second part is unfinished");
    assert!(out.tool_calls.is_empty());
    assert!(out.items.is_empty());
}
#[test]
fn empty_budget_terminal_does_not_drop_streamed_tail_after_a_done_item() {
    let out = decode_sse(&stream(false)).unwrap();
    assert_eq!(out.finish_reason.as_deref(), Some("length"));
    assert_eq!(out.text, "First part. Second part is unfinished");
}

#[test]
fn indexed_truncation_merges_done_parts_and_interleaved_utf8_without_text_deduplication() {
    for prefix_delta in [false, true] {
        let mut accumulator = astra_core::model_wire::responses::ResponseAccumulator::default();
        if prefix_delta {
            accumulator.observe(&json!({"type":"response.output_text.delta","output_index":7,"content_index":0,"delta":"相同"})).unwrap();
        }
        accumulator.observe(&json!({"type":"response.output_item.done","output_index":7,"item":message("prefix","相同","completed")})).unwrap();
        for (item, part, text) in [(42, 0, "相"), (42, 1, "尾"), (42, 0, "同")] {
            accumulator.observe(&json!({"type":"response.output_text.delta","output_index":item,"content_index":part,"delta":text})).unwrap();
        }
        accumulator
            .observe(&terminal("incomplete", json!([])))
            .unwrap();
        let out = accumulator.finish().unwrap();
        assert_eq!(out.text, "相同相同尾");
        assert_eq!(out.finish_reason.as_deref(), Some("length"));
        assert!(out.items.is_empty() && out.tool_calls.is_empty());
    }
}

#[test]
fn ambiguous_unindexed_tail_fails_without_erasing_observations() {
    let mut accumulator = astra_core::model_wire::responses::ResponseAccumulator::default();
    accumulator.observe(&json!({"type":"response.output_item.done","output_index":0,"item":message("prefix","first","completed")})).unwrap();
    accumulator
        .observe(&json!({"type":"response.output_text.delta","delta":"unowned tail"}))
        .unwrap();
    assert!(
        accumulator
            .observe(&terminal("incomplete", json!([])))
            .is_err()
    );
    assert_eq!(accumulator.output.text, "unowned tail");
    assert_eq!(accumulator.output.usage["completion_tokens"], 512);
}

#[test]
fn indexed_text_storage_rejects_resource_exhaustion_without_allocating_by_index() {
    use astra_core::model_wire::responses::ResponseAccumulator;
    let mut accumulator = ResponseAccumulator::default();
    for index in 0..4096_u64 {
        accumulator.observe(&json!({"type":"response.output_text.delta","output_index":index,"content_index":u64::MAX,"delta":"x"})).unwrap();
    }
    accumulator.observe(&json!({"type":"response.output_text.delta","output_index":u64::MAX,"content_index":0,"delta":"x"})).unwrap();
    assert_eq!(accumulator.output.text.len(), 4097);
    assert!(
        accumulator
            .observe(&terminal("incomplete", json!([])))
            .is_err()
    );
    accumulator
        .observe(&terminal(
            "completed",
            json!([message("final", "authoritative", "completed")]),
        ))
        .unwrap();
    assert_eq!(accumulator.finish().unwrap().text, "authoritative");
    let mut accumulator = ResponseAccumulator::default();
    assert!(
        accumulator
            .observe(
                &json!({"type":"response.output_text.delta","delta":"x".repeat(16*1024*1024+1)})
            )
            .is_err()
    );
    assert!(accumulator.output.text.is_empty());
    let mut accumulator = ResponseAccumulator::default();
    let parts = vec![json!({"type":"output_text","text":"x"}); 4097];
    accumulator.observe(&json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":parts}})).unwrap();
    assert!(
        accumulator
            .observe(&terminal("incomplete", json!([])))
            .is_err()
    );
}

#[test]
fn populated_terminal_does_not_require_partial_tracking() {
    let parts = vec![json!({"type":"output_text","text":"x"}); 4097];
    let end = terminal(
        "incomplete",
        json!([{"type":"message","role":"assistant","content":parts}]),
    );
    let delta = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"old delta must be replaced"});
    let out = decode_sse(&format!("data: {delta}\n\ndata: {end}\n\n")).unwrap();
    assert_eq!(out.text, "x".repeat(4097));
    assert!(out.tool_calls.is_empty() && out.items.is_empty());
}
