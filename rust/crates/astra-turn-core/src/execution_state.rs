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
            .clamp(1, 20),
        _ => 10,
    }
}

fn normalize_outcome(data: &Map<String, Value>) -> Map<String, Value> {
    let status = match data.get("status").and_then(Value::as_str) {
        Some(s @ ("success" | "failure" | "exhausted")) => s.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- normalize_round ---

    #[test]
    fn round_none() {
        assert_eq!(normalize_round(None), 0);
    }

    #[test]
    fn round_positive_i64() {
        assert_eq!(normalize_round(Some(&json!(5))), 5);
    }

    #[test]
    fn round_zero() {
        assert_eq!(normalize_round(Some(&json!(0))), 0);
    }

    #[test]
    fn round_negative_clamped() {
        assert_eq!(normalize_round(Some(&json!(-3))), 0);
    }

    #[test]
    fn round_non_number() {
        assert_eq!(normalize_round(Some(&json!("five"))), 0);
    }

    #[test]
    fn round_float_truncated() {
        // json!(3.9) is a float, as_i64() returns None, as_u64() returns None → 0
        assert_eq!(normalize_round(Some(&json!(3.9))), 0);
    }

    // --- normalize_max_rounds ---

    #[test]
    fn max_rounds_none_defaults_10() {
        assert_eq!(normalize_max_rounds(None), 10);
    }

    #[test]
    fn max_rounds_within_range() {
        assert_eq!(normalize_max_rounds(Some(&json!(15))), 15);
    }

    #[test]
    fn max_rounds_above_cap() {
        assert_eq!(normalize_max_rounds(Some(&json!(50))), 20);
    }

    #[test]
    fn max_rounds_below_floor() {
        // 3.min(20) = 3, no lower clamp in code!
        assert_eq!(normalize_max_rounds(Some(&json!(3))), 3);
    }

    #[test]
    fn max_rounds_non_number() {
        assert_eq!(normalize_max_rounds(Some(&json!(null))), 10);
    }

    // --- normalize_outcome ---

    #[test]
    fn outcome_valid_statuses() {
        for status in &["success", "failure", "exhausted"] {
            let data = Map::from_iter([("status".to_string(), json!(status))]);
            let out = normalize_outcome(&data);
            assert_eq!(out["status"].as_str().unwrap(), *status);
        }
    }

    #[test]
    fn outcome_invalid_status_defaults_failure() {
        let data = Map::from_iter([("status".to_string(), json!("unknown"))]);
        let out = normalize_outcome(&data);
        assert_eq!(out["status"].as_str().unwrap(), "failure");
    }

    #[test]
    fn outcome_missing_status() {
        let out = normalize_outcome(&Map::new());
        assert_eq!(out["status"].as_str().unwrap(), "failure");
        assert_eq!(out["content"].as_str().unwrap(), "");
        assert!(out["failure_reason"].is_null());
        assert!(out["failed_tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn outcome_with_content_and_failed_tools() {
        let data = Map::from_iter([
            ("status".to_string(), json!("success")),
            ("content".to_string(), json!("done")),
            ("failed_tools".to_string(), json!(["tool_a"])),
            ("failure_reason".to_string(), json!("timeout")),
        ]);
        let out = normalize_outcome(&data);
        assert_eq!(out["content"].as_str().unwrap(), "done");
        assert_eq!(out["failed_tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["failure_reason"].as_str().unwrap(), "timeout");
    }

    // --- normalize_execution_state (integration) ---

    #[test]
    fn exec_state_empty_input() {
        let out = normalize_execution_state(&Map::new());
        assert!(out["blocked_tools"].as_array().unwrap().is_empty());
        assert!(out["tool_failures"].as_object().unwrap().is_empty());
        assert_eq!(out["round"].as_i64().unwrap(), 0);
        assert_eq!(out["max_rounds"].as_i64().unwrap(), 10);
        assert!(out["outcome"].is_null());
    }

    #[test]
    fn exec_state_blocked_tools_sorted_deduped() {
        let data = Map::from_iter([(
            "blocked_tools".to_string(),
            json!(["zeta", "alpha", "zeta", "beta"]),
        )]);
        let out = normalize_execution_state(&data);
        let tools: Vec<&str> = out["blocked_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(tools, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn exec_state_blocked_tools_filters_non_strings() {
        let data = Map::from_iter([(
            "blocked_tools".to_string(),
            json!(["real", 42, null, "tool"]),
        )]);
        let out = normalize_execution_state(&data);
        let tools: Vec<&str> = out["blocked_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(tools, vec!["real", "tool"]);
    }

    #[test]
    fn exec_state_with_outcome() {
        let data = Map::from_iter([
            ("round".to_string(), json!(3)),
            ("max_rounds".to_string(), json!(15)),
            (
                "outcome".to_string(),
                json!({"status": "success", "content": "ok"}),
            ),
        ]);
        let out = normalize_execution_state(&data);
        assert_eq!(out["round"].as_i64().unwrap(), 3);
        assert_eq!(out["max_rounds"].as_i64().unwrap(), 15);
        let outcome = out["outcome"].as_object().unwrap();
        assert_eq!(outcome["status"].as_str().unwrap(), "success");
    }

    #[test]
    fn exec_state_tool_failures_preserved() {
        let data = Map::from_iter([("tool_failures".to_string(), json!({"bash": 2, "read": 1}))]);
        let out = normalize_execution_state(&data);
        let failures = out["tool_failures"].as_object().unwrap();
        assert_eq!(failures.len(), 2);
    }

    /// P0-C: max_rounds must have a floor of 1. Zero or negative values
    /// would make the agentic loop condition `round < max_rounds` always
    /// false, silently disabling the entire loop.
    #[test]
    fn max_rounds_zero_normalized_to_at_least_one() {
        let data = Map::from_iter([("max_rounds".to_string(), json!(0))]);
        let out = normalize_execution_state(&data);
        let max = out["max_rounds"].as_i64().unwrap();
        assert!(max >= 1, "max_rounds=0 must be normalized to ≥1, got {max}");
    }

    #[test]
    fn max_rounds_negative_normalized_to_at_least_one() {
        let data = Map::from_iter([("max_rounds".to_string(), json!(-5))]);
        let out = normalize_execution_state(&data);
        let max = out["max_rounds"].as_i64().unwrap();
        assert!(
            max >= 1,
            "max_rounds=-5 must be normalized to ≥1, got {max}"
        );
    }

    #[test]
    fn max_rounds_one_is_valid() {
        let data = Map::from_iter([("max_rounds".to_string(), json!(1))]);
        let out = normalize_execution_state(&data);
        assert_eq!(out["max_rounds"].as_i64().unwrap(), 1);
    }

    #[test]
    fn max_rounds_upper_cap_still_works() {
        let data = Map::from_iter([("max_rounds".to_string(), json!(100))]);
        let out = normalize_execution_state(&data);
        assert_eq!(out["max_rounds"].as_i64().unwrap(), 20);
    }
}
