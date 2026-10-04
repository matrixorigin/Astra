pub(crate) use astra_turn_core::orchestration::agent_result_wire::AgentToolWireOutcomeKind as AgentControlWireOutcomeKind;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_error_message as agent_control_error_message;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_interrupted_message as agent_control_interrupted_message;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_result_output_summary as agent_control_result_output_summary;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::agent_tool_running_preview as agent_control_running_preview;
pub(crate) use astra_turn_core::orchestration::agent_result_wire::project_agent_tool_wire as project_agent_control_wire;

pub(crate) fn delegation_target(name: &str, arguments: &str) -> Option<String> {
    if matches!(name, "agent" | "agent_fanout") {
        return Some(name.to_string());
    }
    if name != "invoke_tool" {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("name")?
        .as_str()
        .filter(|target| matches!(*target, "agent" | "agent_fanout"))
        .map(str::to_string)
}

pub(crate) fn compact_delegation_description(name: &str, arguments: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let action = value
        .as_ref()
        .and_then(|value| {
            if name == "invoke_tool" {
                value.get("arguments")
            } else {
                Some(value)
            }
        })
        .and_then(|value| value.get("action"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| (name == "agent_fanout").then(|| "fanout".to_string()));
    let action = match action.as_deref() {
        Some("spawn") => "spawn",
        Some("get_result") => "result",
        Some("list") => "list",
        Some("send_message") => "message",
        Some("fanout") => "fanout",
        _ => "control",
    };
    format!("Agent {action} · Ctrl+G agents")
}

pub(crate) fn compact_delegation_result(status: Option<&str>, payload: Option<&str>) -> String {
    let parsed = payload
        .and_then(astra_turn_core::orchestration::agent_result_wire::agent_control_result_value);
    let wire_status = if matches!(
        status,
        Some("failed" | "rejected" | "cancelled" | "uncertain")
    ) {
        status.unwrap()
    } else {
        parsed
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str)
            .or(status)
            .unwrap_or("completed")
    };
    let skipped = payload.is_some_and(|payload| {
        astra_turn_core::orchestration::agent_result_wire::agent_fanout_control_receipt_kind(payload)
            == Some(astra_turn_core::orchestration::agent_result_wire::AgentFanoutControlReceiptKind::SkippedBeforeAcceptance)
    });
    let slot_failure = parsed.as_ref().and_then(|value| {
        ["agents", "results"].into_iter().find_map(|field| {
            value.get(field)?.as_array()?.iter().find_map(|slot| {
                let outcome = slot
                    .get("result")
                    .filter(|result| result.is_object())
                    .unwrap_or(slot);
                matches!(
                    outcome.get("status")?.as_str()?,
                    "failed" | "spawn_rejected" | "rejected" | "interrupted" | "cancelled"
                )
                .then(|| outcome.get("error")?.as_str())
                .flatten()
            })
        })
    });
    let status = if skipped {
        "not started"
    } else if slot_failure.is_some() {
        "partial failure"
    } else {
        match wire_status {
            "launched" => "launched",
            "completed" | "success" => "completed",
            "completed_with_issues" => "completed with issues",
            "failed" | "error" | "spawn_rejected" => "failed",
            "skipped_before_acceptance" => "not started",
            "cancelled" => "cancelled",
            "interrupted" => "interrupted",
            "rejected" => "rejected",
            "uncertain" => "uncertain",
            _ => "updated",
        }
    };
    let detail = matches!(
        status,
        "failed" | "rejected" | "not started" | "interrupted" | "partial failure"
    )
    .then(|| {
        slot_failure.or_else(|| {
            parsed.as_ref().and_then(|value| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| value.get("reason")?.as_str())
                    .or_else(|| value.get("advisory")?.get("next_step")?.as_str())
            })
        })
    })
    .flatten()
    .map(|value| {
        astra_text_utils::credential_redaction::redact_credentials_for_display(value)
            .0
            .chars()
            .filter(|ch| !ch.is_control())
            .take(160)
            .collect::<String>()
    })
    .filter(|value| !value.trim().is_empty());
    match detail {
        Some(detail) => format!("Agent control: {status} · {detail}"),
        None => format!("Agent control: {status} · Ctrl+G agents"),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::agent_control_interrupted_message;
    use super::{compact_delegation_result, delegation_target};
    use astra_turn_core::orchestration::agent_result_wire::AgentToolResultStatusKind;

    #[test]
    fn shared_agent_control_status_kind_roundtrips_via_from_str() {
        assert_eq!(
            AgentToolResultStatusKind::from_str("interrupted").unwrap(),
            AgentToolResultStatusKind::Interrupted
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("still_running").unwrap(),
            AgentToolResultStatusKind::StillRunning
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("launched").unwrap(),
            AgentToolResultStatusKind::Launched
        );
        assert!(AgentToolResultStatusKind::from_str("weird").is_err());
    }

    #[test]
    fn interrupted_message_uses_shared_wait_copy_for_get_result() {
        assert_eq!(
            agent_control_interrupted_message(true, Some("budget_exhausted")),
            "Needs continuation: The run reached its turn budget."
        );
        assert_eq!(
            agent_control_interrupted_message(false, Some("context_overflow")),
            "Needs compaction: The conversation exceeded the model context window."
        );
        assert_eq!(
            agent_control_interrupted_message(false, None),
            "Agent stopped before completing its result."
        );
    }

    #[test]
    fn rejected_delegation_keeps_a_bounded_reason_without_child_output() {
        let payload = serde_json::json!({
            "status": "spawn_rejected",
            "error": "requested model is unavailable",
            "result": "private child output"
        })
        .to_string();
        let summary = compact_delegation_result(Some("failed"), Some(&payload));
        assert!(summary.contains("requested model is unavailable"));
        assert!(!summary.contains("private child output"));
    }

    #[test]
    fn deferred_delegation_target_and_failure_receipts_are_projected() {
        assert_eq!(
            delegation_target(
                "invoke_tool",
                r#"{"name":"agent","arguments":{"action":"spawn","prompt":"private"}}"#,
            ),
            Some("agent".to_string())
        );
        assert_eq!(
            delegation_target("invoke_tool", r#"{"name":"web_fetch"}"#),
            None
        );
        assert!(compact_delegation_result(
            None,
            Some(r#"{"status":"completed","outcome":"delegation_skipped","reason_code":"insufficient_time_to_delegate","executed":false}"#)
        ).contains("not started"));
        assert!(
            compact_delegation_result(
                None,
                Some(r#"{"status":"rejected","reason":"recipient unavailable"}"#)
            )
            .contains("recipient unavailable")
        );
        assert!(compact_delegation_result(
            None,
            Some(r#"{"status":"started","agents":[{"status":"failed","error":"model denied","result":"private"}]}"#)
        ).contains("model denied"));
        assert!(!compact_delegation_result(
            None,
            Some(r#"{"status":"started","agents":[{"status":"failed","error":"model denied","result":"private"}]}"#)
        ).contains("private"));
        assert!(compact_delegation_result(
            None,
            Some(r#"{"status":"completed_with_issues","results":[{"result":{"status":"failed","error":"child unavailable","content":"private"}}]}"#)
        ).contains("child unavailable"));
        assert!(
            !compact_delegation_result(Some("failed"), Some("private child result"))
                .contains("private child result")
        );
    }
}
