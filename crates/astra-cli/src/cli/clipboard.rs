//! Local clipboard process boundary for CLI commands and the TUI.

use std::{
    io::Write,
    process::{Command as SysCommand, Stdio},
};

pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    // Try Wayland first (if WAYLAND_DISPLAY is set), then X11 tools, then macOS.
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"] as &[&str]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ]);
    let mut attempts = Vec::new();
    for (cmd, args) in &candidates {
        let mut child = match SysCommand::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                attempts.push(format!("{cmd}: failed to start: {error}"));
                continue;
            }
        };
        match child.stdin.take() {
            Some(mut stdin) => {
                if let Err(error) = stdin.write_all(text.as_bytes()) {
                    attempts.push(format!("{cmd}: failed to write clipboard payload: {error}"));
                    if let Err(wait_error) = child.wait() {
                        attempts.push(format!(
                            "{cmd}: failed to reap after write error: {wait_error}"
                        ));
                    }
                    continue;
                }
            }
            None => {
                attempts.push(format!("{cmd}: stdin pipe was unavailable"));
                if let Err(wait_error) = child.wait() {
                    attempts.push(format!(
                        "{cmd}: failed to reap after missing stdin: {wait_error}"
                    ));
                }
                continue;
            }
        }
        match child.wait() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => attempts.push(format!("{cmd}: exited with status {status}")),
            Err(error) => attempts.push(format!("{cmd}: failed to wait for process: {error}")),
        }
    }
    Err(format!("clipboard unavailable ({})", attempts.join("; ")))
}

const CLIPBOARD_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Copy without ever blocking the TUI event loop. Each candidate is bounded,
/// killed and reaped on timeout before the next platform command is tried.
pub(crate) async fn copy_to_clipboard_async(text: String) -> Result<(), String> {
    let wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
    let mut candidates: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        candidates.push(("wl-copy", &[]));
    }
    candidates.extend_from_slice(&[
        ("xclip", &["-selection", "clipboard"] as &[&str]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ]);

    let mut attempts = Vec::new();
    for (command, args) in candidates {
        match run_clipboard_candidate(command, args, &text, CLIPBOARD_ATTEMPT_TIMEOUT).await {
            Ok(()) => return Ok(()),
            Err(error) => attempts.push(format!("{command}: {error}")),
        }
    }
    Err(format!("clipboard unavailable ({})", attempts.join("; ")))
}

async fn run_clipboard_candidate(
    command: &str,
    args: &[&str],
    text: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to start: {error}"))?;

    let attempt = async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "stdin pipe was unavailable".to_string())?;
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|error| format!("failed to write payload: {error}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| format!("failed to close payload: {error}"))?;
        drop(stdin);
        child
            .wait()
            .await
            .map_err(|error| format!("failed to wait: {error}"))
    };

    match tokio::time::timeout(timeout, attempt).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("exited with status {status}")),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(format!("timed out after {}ms", timeout.as_millis()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_clipboard_candidate;

    #[cfg(unix)]
    #[tokio::test]
    async fn clipboard_candidate_has_a_hard_process_deadline() {
        let started = std::time::Instant::now();
        let result = run_clipboard_candidate(
            "sh",
            &["-c", "exec sleep 30"],
            "payload",
            std::time::Duration::from_millis(20),
        )
        .await;

        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clipboard_candidate_streams_payload_and_reaps_success() {
        run_clipboard_candidate(
            "sh",
            &["-c", "cat >/dev/null"],
            "payload",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    }
}
