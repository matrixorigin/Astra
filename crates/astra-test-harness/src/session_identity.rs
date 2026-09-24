//! Exact server-session identity and convergence helpers for harness runs.
//!
//! A harness must only control a session after the running CLI observed and
//! reported the server-issued identity. Guessing from a session list is unsafe
//! in multi-user and parallel test environments.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

const SESSION_CANCEL_TOTAL_TIMEOUT: Duration = Duration::from_secs(25);
const SESSION_CANCEL_RETRY_DELAY: Duration = Duration::from_millis(250);

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
    cancel_server_session_with_timeout(astra_bin, profile, session_id, SESSION_CANCEL_TOTAL_TIMEOUT)
        .await
}

async fn cancel_server_session_with_timeout(
    astra_bin: &Path,
    profile: Option<&str>,
    session_id: &str,
    total_timeout: Duration,
) -> Result<(), String> {
    if !is_valid_server_session_id(session_id) {
        return Err("refusing to cancel invalid server session id".into());
    }

    let deadline = tokio::time::Instant::now() + total_timeout;
    let mut pending_retries = 0u32;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("session cancellation exceeded its bounded cleanup window".into());
        }
        let mut command = Command::new(astra_bin);
        if let Some(profile) = profile {
            command.arg("--profile").arg(profile);
        }
        command
            .args(["session", "cancel", session_id])
            .kill_on_drop(true);
        let output = tokio::time::timeout(remaining, command.output())
            .await
            .map_err(|_| "session cancellation exceeded its bounded cleanup window".to_string())
            .and_then(|result| {
                result.map_err(|error| format!("failed to spawn session cancel: {error}"))
            })?;
        if !output.status.success() {
            let error = format!(
                "session cancel exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            if output.status.code() == Some(7) {
                pending_retries += 1;
                if pending_retries == 1 {
                    eprintln!(
                        "[astra-test] session cancellation is pending; retrying within the cleanup deadline"
                    );
                }
                let retry_delay = SESSION_CANCEL_RETRY_DELAY
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                tokio::time::sleep(retry_delay).await;
                continue;
            }
            return Err(error);
        }
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                format!(
                    "session cancel did not return JSON ({error}): {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                )
            })?;
        validate_cancellation_response(&response, session_id)?;
        if pending_retries > 0 {
            eprintln!(
                "[astra-test] session cancellation converged after {pending_retries} pending retries"
            );
        }
        return Ok(());
    }
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
        .kill_on_drop(true);
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
        cancel_server_session, cancel_server_session_with_timeout, is_valid_server_session_id,
        run_id_from_stream_event, session_id_from_stream_event,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_retries_typed_pending_until_settled_or_total_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("astra-cancel-shim");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
attempt_file="${0}.attempts"
attempt=0
if [ -f "$attempt_file" ]; then attempt=$(cat "$attempt_file"); fi
attempt=$((attempt + 1))
printf '%s\n' "$attempt" > "$attempt_file"
if [ "$attempt" -le 2 ] || [ "$3" = "ed1d08ae-b89a-40b5-ae81-07f515b4e620" ]; then
  printf 'session %s cancellation has not been confirmed complete: an execution is still stopping. Retry `astra session cancel %s`; keep the session history\n' "$3" "$3" >&2
  exit 7
fi
printf '{"session_id":"%s","status":"cancelled","execution_settled":true}\n' "$3"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        cancel_server_session(&executable, None, SESSION_ID)
            .await
            .unwrap();

        let attempts =
            std::fs::read_to_string(format!("{}.attempts", executable.display())).unwrap();
        assert_eq!(attempts.trim(), "3");

        std::fs::write(format!("{}.attempts", executable.display()), "0").unwrap();
        let other_session = "ed1d08ae-b89a-40b5-ae81-07f515b4e620";
        let error = cancel_server_session_with_timeout(
            &executable,
            None,
            other_session,
            std::time::Duration::from_millis(800),
        )
        .await
        .expect_err("persistent pending settlement must exhaust the total deadline");
        assert!(error.contains("bounded cleanup window"));
        let attempts =
            std::fs::read_to_string(format!("{}.attempts", executable.display())).unwrap();
        assert!(attempts.trim().parse::<u32>().unwrap() >= 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_total_timeout_kills_a_stuck_cli_process() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("astra-cancel-stuck-shim");
        std::fs::write(&executable, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let started = tokio::time::Instant::now();
        let error = cancel_server_session_with_timeout(
            &executable,
            None,
            SESSION_ID,
            Duration::from_millis(100),
        )
        .await
        .expect_err("a stuck CLI must not outlive the cleanup deadline");

        assert!(error.contains("bounded cleanup window"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_does_not_retry_non_pending_failure() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("astra-cancel-failure-shim");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf x >> \"${0}.attempts\"\nprintf 'unauthorized\\n' >&2\nexit 3\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let error = cancel_server_session(&executable, None, SESSION_ID)
            .await
            .expect_err("non-pending failures cannot be retried");
        assert!(error.contains("unauthorized"));
        let attempts =
            std::fs::read_to_string(format!("{}.attempts", executable.display())).unwrap();
        assert_eq!(attempts, "x");
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
