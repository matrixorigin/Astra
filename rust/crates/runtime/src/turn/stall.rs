use std::collections::BTreeSet;

use serde_json::Value;

pub const SERVER_STALL_WINDOW: usize = 3;

pub fn canonical_tool_args(raw: &str) -> String {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => serde_json::to_string(&value).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

pub fn server_tool_call_signature(tool_calls: &[Value]) -> BTreeSet<String> {
    tool_calls
        .iter()
        .map(|tool_call| {
            let function = tool_call
                .get("function")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{name}:{}", canonical_tool_args(arguments))
        })
        .collect()
}

pub fn record_server_tool_signatures(
    tool_sigs: &mut Vec<BTreeSet<String>>,
    tool_calls: &[Value],
    window: usize,
) {
    if tool_calls.is_empty() {
        tool_sigs.clear();
        return;
    }

    tool_sigs.push(server_tool_call_signature(tool_calls));
    if tool_sigs.len() > window {
        let drain_count = tool_sigs.len() - window;
        tool_sigs.drain(0..drain_count);
    }
}

pub fn detect_server_stall(tool_sigs: &[BTreeSet<String>], window: usize) -> bool {
    if tool_sigs.len() < window {
        return false;
    }

    let recent = &tool_sigs[tool_sigs.len() - window..];
    recent.iter().all(|sig| sig == &recent[window - 1])
}
