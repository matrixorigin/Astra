//! Exact server-session identity and convergence helpers for harness runs.
//!
//! A harness must only control a session after the running CLI observed and
//! reported the server-issued identity. Guessing from a session list is unsafe
//! in multi-user and parallel test environments.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Extract the server-issued id from the CLI's structured lifecycle stream.
///
/// Ignore every other stderr line, including malformed JSON and user/model
/// text. A UUID check makes this a producer identity handoff rather than a
/// substring match over diagnostics.
pub(crate) fn session_id_from_stream_event(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_bound") {
        return None;
    }
    let session_id = value.get("session_id")?.as_str()?;
    if !is_valid_server_session_id(session_id) {
        return None;
    }
    Some(session_id.to_owned())
}

/// Extract the server-issued run identity from the CLI lifecycle stream.
///
/// A timeout has no terminal JSON envelope, but the stream emits `run_bound`
/// before provider work starts. The harness must retain that identity so
/// durable evidence can be scoped to this invocation instead of being
/// discarded as an unrelated/resumed-session transcript.
pub(crate) fn run_id_from_stream_event(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("run_bound") {
        return None;
    }
    let run_id = value.get("run_id")?.as_str()?;
    if !is_valid_server_session_id(run_id) {
        return None;
    }
    Some(run_id.to_owned())
}

/// Validate the only session identity the current server contract emits.
///
/// Session IDs are producer-owned UUIDs, not arbitrary path-safe strings. The
/// harness uses this same predicate both when extracting an identity and when
/// deciding whether a follow-up may reuse it, so an executor cannot make an
/// invalid-but-equal identity look like a valid continuation.
pub(crate) fn is_valid_server_session_id(session_id: &str) -> bool {
    uuid::Uuid::parse_str(session_id).is_ok()
}

/// Cancel exactly one harness-owned session through the normal authenticated
/// CLI surface. The CLI waits for server-confirmed execution settlement;
/// `session close` only changes display status and cannot supply this proof.
pub(crate) async fn cancel_server_session(
    astra_bin: &Path,
    profile: Option<&str>,
    session_id: &str,
) -> Result<(), String> {
    if !is_valid_server_session_id(session_id) {
        return Err("refusing to cancel invalid server session id".into());
    }

    let mut command = Command::new(astra_bin);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    command
        .args(["session", "cancel", session_id])
        .kill_on_drop(true)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1");
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| "session cancellation timed out after 15s".to_string())
        .and_then(|result| {
            result.map_err(|error| format!("failed to spawn session cancel: {error}"))
        })?;
    if !output.status.success() {
        return Err(format!(
            "session cancel exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "session cancel did not return JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })?;
    validate_cancellation_response(&response, session_id)
}

fn validate_cancellation_response(
    response: &serde_json::Value,
    session_id: &str,
) -> Result<(), String> {
    if response
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        != Some(session_id)
        || response.get("status").and_then(serde_json::Value::as_str) != Some("cancelled")
        || response
            .get("execution_settled")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        return Err(format!(
            "session cancel did not converge to cancelled: {}",
            response
        ));
    }
    Ok(())
}

/// Cancel and then delete exactly one harness-owned session through the normal
/// authenticated CLI surface. Cancellation settles active execution; deletion
/// is harness-owned history cleanup, not a prerequisite for checkout reuse.
pub(crate) async fn delete_server_session(
    astra_bin: &Path,
    profile: Option<&str>,
    session_id: &str,
) -> Result<(), String> {
    cancel_server_session(astra_bin, profile, session_id).await?;

    let mut command = Command::new(astra_bin);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    command
        .args(["session", "delete", session_id])
        .kill_on_drop(true)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1");
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| "session deletion timed out after 15s".to_string())
        .and_then(|result| {
            result.map_err(|error| format!("failed to spawn session delete: {error}"))
        })?;
    if !output.status.success() {
        return Err(format!(
            "session delete exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_valid_server_session_id, run_id_from_stream_event, session_id_from_stream_event,
    };

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn cancellation_requires_exact_identity_and_explicit_settlement() {
        let valid = serde_json::json!({
            "session_id": SESSION_ID, "status": "cancelled", "execution_settled": true,
        });
        assert!(super::validate_cancellation_response(&valid, SESSION_ID).is_ok());
        for invalid in [
            serde_json::json!({"session_id": SESSION_ID, "status": "cancelled"}),
            serde_json::json!({"session_id": SESSION_ID, "status": "cancelled", "execution_settled": false}),
            serde_json::json!({"session_id": SESSION_ID, "status": "cancelling", "execution_settled": false}),
            serde_json::json!({"session_id": "another-session", "status": "cancelled", "execution_settled": true}),
        ] {
            assert!(super::validate_cancellation_response(&invalid, SESSION_ID).is_err());
        }
    }

    #[test]
    fn accepts_only_typed_server_session_binding_events() {
        assert_eq!(
            session_id_from_stream_event(&format!(
                r#"{{"type":"session_bound","session_id":"{SESSION_ID}"}}"#
            )),
            Some(SESSION_ID.into())
        );
        assert_eq!(
            session_id_from_stream_event(&format!(
                r#"{{"type":"tool_completed","session_id":"{SESSION_ID}"}}"#
            )),
            None
        );
        assert_eq!(
            session_id_from_stream_event(r#"{"type":"session_bound","session_id":"not-a-uuid"}"#),
            None
        );
    }

    #[test]
    fn identity_validator_rejects_equal_but_non_server_ids() {
        assert!(is_valid_server_session_id(SESSION_ID));
        for invalid in ["", "sess-m", "../escape", "not-a-uuid"] {
            assert!(!is_valid_server_session_id(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn run_bound_stream_identity_is_typed_and_unambiguous() {
        let run_id = "8a0dcb50-38a7-4402-bef3-2c1aee9a4e85";
        assert_eq!(
            run_id_from_stream_event(&format!(r#"{{"type":"run_bound","run_id":"{run_id}"}}"#)),
            Some(run_id.into())
        );
        assert_eq!(
            run_id_from_stream_event(r#"{"type":"run_bound","run_id":"run-1"}"#),
            None
        );
    }
}
