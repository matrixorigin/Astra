use serde_json::{Map, Value};

pub fn normalize_execution_state(data: &Map<String, Value>) -> Map<String, Value> {
    let blocked_tools = data
        .get("blocked_tools")
        .and_then(Value::as_array)
        .map(|tools| {
            let mut names = tools
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            Value::Array(names.into_iter().map(Value::String).collect())
        })
        .unwrap_or_else(|| Value::Array(Vec::new()));

    let tool_failures = data
        .get("tool_failures")
        .and_then(Value::as_object)
        .map(|failures| Value::Object(failures.clone()))
        .unwrap_or_else(|| Value::Object(Map::new()));

    let round = normalize_round(data.get("round"));
    let max_rounds = normalize_max_rounds(data.get("max_rounds"));
    let outcome = data
        .get("outcome")
        .and_then(Value::as_object)
        .map(normalize_outcome)
        .map(Value::Object)
        .unwrap_or(Value::Null);

    Map::from_iter([
        ("blocked_tools".to_string(), blocked_tools),
        ("tool_failures".to_string(), tool_failures),
        ("round".to_string(), Value::from(round)),
        ("max_rounds".to_string(), Value::from(max_rounds)),
        ("outcome".to_string(), outcome),
    ])
}

fn normalize_round(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .unwrap_or(0)
            .max(0),
        _ => 0,
    }
}

fn normalize_max_rounds(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .unwrap_or(10)
            .min(20),
        _ => 10,
    }
}

fn normalize_outcome(data: &Map<String, Value>) -> Map<String, Value> {
    let status = match data.get("status").and_then(Value::as_str) {
        Some("success" | "failure" | "exhausted") => data
            .get("status")
            .and_then(Value::as_str)
            .unwrap()
            .to_string(),
        _ => "failure".to_string(),
    };

    let failed_tools = data
        .get("failed_tools")
        .and_then(Value::as_array)
        .map(|tools| Value::Array(tools.clone()))
        .unwrap_or_else(|| Value::Array(Vec::new()));

    let failure_reason = data.get("failure_reason").cloned().unwrap_or(Value::Null);

    Map::from_iter([
        ("status".to_string(), Value::String(status)),
        (
            "content".to_string(),
            Value::String(
                data.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
        ),
        ("failure_reason".to_string(), failure_reason),
        ("failed_tools".to_string(), failed_tools),
    ])
}
