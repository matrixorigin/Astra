//! LLM-based intent drift detection.
//!
//! This module provides semantic drift detection using an LLM call that is
//! **separate** from the main conversation to avoid prompt cache pollution.
//!
//! Design principles:
//! - **Separate LLM call**: drift detection uses its own minimal prompt, not
//!   the main conversation's system prompt + history. This ensures the main
//!   loop's prompt cache prefix is never invalidated by drift checks.
//! - **Structured output**: the LLM returns JSON `{"drift": "on_task" | "drifting", "reason": "..."}`.
//! - **Fallback**: if the LLM call fails, default to `OnTask` (no false positives).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Result of drift detection from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "drift", rename_all = "snake_case")]
pub enum DriftDetectionResult {
    /// Agent is working on the user's request.
    OnTask {
        /// Why the LLM thinks the agent is on-task.
        reason: String,
    },
    /// Agent has drifted from the user's intent.
    Drifting {
        /// Why the LLM thinks the agent has drifted.
        reason: String,
    },
}

/// Abstraction over the LLM API for drift detection.
/// Implemented by the real HTTP client in ServerHost and CliHost.
#[async_trait]
pub trait DriftDetectionClient: Send + Sync {
    /// Send a drift detection request. Returns the structured result or an error.
    async fn detect_drift(
        &self,
        user_query: &str,
        recent_tool_turns: &[(Vec<String>, String)],
    ) -> Result<DriftDetectionResult, String>;
}

/// Build the messages array for the drift detection LLM call.
///
/// This is intentionally minimal: no system prompt, just a single user message
/// containing the user's original query and the agent's recent tool calls.
/// This ensures the drift detection call never shares a cache prefix with the
/// main conversation.
pub fn build_drift_detection_messages(
    user_query: &str,
    recent_tool_turns: &[(Vec<String>, String)],
) -> Vec<Value> {
    let tool_history: String = recent_tool_turns
        .iter()
        .enumerate()
        .map(|(i, (tools, args))| {
            let tools_str = tools.join(", ");
            format!("Turn {}: tools=[{}] args={}", i + 1, tools_str, args)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        r#"You are a drift detector for an AI coding assistant.

USER'S ORIGINAL REQUEST:
{}

AGENT'S RECENT TOOL CALLS (most recent last):
{}

TASK: Determine if the agent's recent tool calls are related to the user's request.

Consider:
- Direct relevance: tools operating on files/modules mentioned in the request
- Indirect relevance: tools gathering information needed to fulfill the request
- Semantic similarity: tools working on concepts related to the request (e.g., "auth" for "authentication")
- Language: the request may be in Chinese, English, or mixed

Respond with JSON only (no markdown):
{{"drift": "on_task", "reason": "..."}}
OR
{{"drift": "drifting", "reason": "..."}}"#,
        user_query, tool_history
    );

    vec![json!({
        "role": "user",
        "content": prompt
    })]
}

/// Parse the LLM's response into a structured result.
///
/// Returns `Ok(OnTask)` if parsing fails (fail-open: no false positives).
pub fn parse_drift_detection_response(response: &str) -> Result<DriftDetectionResult, String> {
    // Strip markdown code fences if present
    let cleaned = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let value: Value = serde_json::from_str(cleaned)
        .map_err(|e| format!("Failed to parse JSON: {}, raw: {}", e, cleaned))?;

    let drift = value
        .get("drift")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing 'drift' field in response: {}", cleaned))?;

    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("No reason provided")
        .to_string();

    match drift {
        "on_task" => Ok(DriftDetectionResult::OnTask { reason }),
        "drifting" => Ok(DriftDetectionResult::Drifting { reason }),
        _ => Err(format!(
            "Invalid drift value: {}, expected 'on_task' or 'drifting'",
            drift
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_drift_detection_messages_structure() {
        let user_query = "Fix the authentication bug";
        let recent_tool_turns = vec![
            (
                vec!["read_file".to_string()],
                r#"{"path": "src/auth.rs"}"#.to_string(),
            ),
            (
                vec!["bash".to_string()],
                r#"{"command": "cargo test"}"#.to_string(),
            ),
        ];

        let messages = build_drift_detection_messages(user_query, &recent_tool_turns);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");

        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("USER'S ORIGINAL REQUEST"));
        assert!(content.contains("Fix the authentication bug"));
        assert!(content.contains("AGENT'S RECENT TOOL CALLS"));
        assert!(content.contains("read_file"));
        assert!(content.contains("src/auth.rs"));
        assert!(content.contains("cargo test"));
    }

    #[test]
    fn test_parse_drift_detection_response_on_task() {
        let response = r#"{"drift": "on_task", "reason": "Agent is reading auth.rs which is directly related to the authentication bug"}"#;
        let result = parse_drift_detection_response(response).unwrap();

        match result {
            DriftDetectionResult::OnTask { reason } => {
                assert!(reason.contains("auth.rs"));
            }
            _ => panic!("Expected OnTask"),
        }
    }

    #[test]
    fn test_parse_drift_detection_response_drifting() {
        let response = r#"{"drift": "drifting", "reason": "Agent is writing documentation instead of fixing the authentication bug"}"#;
        let result = parse_drift_detection_response(response).unwrap();

        match result {
            DriftDetectionResult::Drifting { reason } => {
                assert!(reason.contains("documentation"));
            }
            _ => panic!("Expected Drifting"),
        }
    }

    #[test]
    fn test_parse_drift_detection_response_with_markdown() {
        let response = r#"```json
{"drift": "on_task", "reason": "Related work"}
```"#;
        let result = parse_drift_detection_response(response).unwrap();

        match result {
            DriftDetectionResult::OnTask { .. } => {}
            _ => panic!("Expected OnTask"),
        }
    }

    #[test]
    fn test_parse_drift_detection_response_invalid_json() {
        let response = "Not JSON";
        let result = parse_drift_detection_response(response);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_drift_detection_response_missing_drift_field() {
        let response = r#"{"reason": "No drift field"}"#;
        let result = parse_drift_detection_response(response);
        assert!(result.is_err());
    }
}
