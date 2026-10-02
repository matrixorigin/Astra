//! Contracts for shared tool admission and lossless argument parsing.

use astra_turn_core::parallel_tool_exec::parse_tool_args;
use serde_json::{Value, json};
use std::sync::Arc;

fn tool_call_with_arguments(name: &str, id: &str, arguments: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": arguments }
    })
}

#[test]
fn shared_tool_semaphore_returns_same_instance() {
    use astra_turn_core::parallel_tool_exec::shared_tool_semaphore;
    let a = shared_tool_semaphore();
    let b = shared_tool_semaphore();
    assert!(
        Arc::ptr_eq(&a, &b),
        "shared_tool_semaphore must return the same Arc on repeat calls"
    );
}

#[test]
fn tool_arg_parser_handles_nested_quotes_unicode_and_malformed_fail_closed() {
    let nested = tool_call_with_arguments(
        "bash",
        "q1",
        Value::String(
            json!({
                "command": "printf '%s' \"hello \\\"astra\\\" 边云\"",
                "env": {"GREETING": "hello \"quoted\""},
                "flags": ["--json", "emoji-🚀"]
            })
            .to_string(),
        ),
    );
    let parsed = parse_tool_args(&nested).expect("nested JSON string args should parse");
    assert_eq!(
        parsed["command"], "printf '%s' \"hello \\\"astra\\\" 边云\"",
        "escaped quotes and unicode must survive parsing"
    );
    assert_eq!(parsed["env"]["GREETING"], "hello \"quoted\"");

    let malformed = tool_call_with_arguments(
        "bash",
        "bad",
        Value::String("{\"command\":\"git status\"".into()),
    );
    assert!(parse_tool_args(&malformed).is_none());
}
