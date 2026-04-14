//! Passive `tsc --noEmit` after TypeScript edits when `tsconfig.json` exists.
//!
//! Requires `tsc` on `PATH` (e.g. `npm i -D typescript`). If `tsc` is missing, the
//! pending flag is cleared and nothing is injected (no noisy errors).
//!
//! Kill switch: `ASTRA_PASSIVE_TSC_CHECK=0|false|off`
//! Timeout: `ASTRA_PASSIVE_TSC_TIMEOUT_SECS` (default 90, max 300)

#![allow(dead_code)]
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

/// Keep in sync with [`super::passive_cargo_check`] budget.
const MAX_PASSIVE_DIAG_CHARS: usize = 12_000;

fn passive_tsc_check_enabled() -> bool {
    match std::env::var("ASTRA_PASSIVE_TSC_CHECK") {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "off")
        }
        Err(_) => true,
    }
}

fn passive_tsc_timeout() -> Duration {
    let secs = std::env::var("ASTRA_PASSIVE_TSC_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(90);
    Duration::from_secs(secs.clamp(1, 300))
}

fn is_ts_like_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx")
    )
}

/// Whether a successful disk write should schedule passive `tsc --noEmit`.
#[must_use]
pub fn should_schedule_passive_tsc(project_root: &Path, edited_path: &Path) -> bool {
    if !passive_tsc_check_enabled() {
        return false;
    }
    if !is_ts_like_source(edited_path) {
        return false;
    }
    project_root.join("tsconfig.json").is_file()
}

fn truncate_body(s: &str) -> String {
    if s.chars().count() <= MAX_PASSIVE_DIAG_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_PASSIVE_DIAG_CHARS).collect();
    format!("{head}\n\n[truncated passive tsc diagnostics at {MAX_PASSIVE_DIAG_CHARS} chars]")
}

fn diagnostic_message(content: String) -> Value {
    json!({
        "role": "user",
        "content": content,
        "attachment_metadata": {
            "kind": "passive_workspace_diagnostics",
            "source": "tsc_no_emit",
        }
    })
}

/// Drain pending, run `tsc --noEmit -p tsconfig.json` when appropriate.
pub async fn take_passive_tsc_messages(
    pending: &AtomicBool,
    project_root: &Path,
    tool_results_nonempty: bool,
) -> Vec<Value> {
    if !passive_tsc_check_enabled() || !tool_results_nonempty {
        return Vec::new();
    }
    if !pending.load(Ordering::SeqCst) {
        return Vec::new();
    }
    if !project_root.join("tsconfig.json").is_file() {
        pending.store(false, Ordering::SeqCst);
        return Vec::new();
    }
    pending.store(false, Ordering::SeqCst);

    let run = async {
        Command::new("tsc")
            .args(["--noEmit", "-p", "tsconfig.json"])
            .current_dir(project_root)
            .kill_on_drop(true)
            .output()
            .await
    };

    let output = match timeout(passive_tsc_timeout(), run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == ErrorKind::NotFound => {
            // TypeScript not installed / not on PATH — skip silently.
            return Vec::new();
        }
        Ok(Err(e)) => {
            return vec![diagnostic_message(format!(
                "Passive `tsc --noEmit` could not run: {e}"
            ))];
        }
        Err(_) => {
            return vec![diagnostic_message(format!(
                "Passive `tsc --noEmit` timed out after {:?}",
                passive_tsc_timeout()
            ))];
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");
    let code = output.status.code();
    if matches!(code, Some(0)) {
        return Vec::new();
    }

    let body = truncate_body(combined.trim());
    vec![diagnostic_message(format!(
        "<new-diagnostics>\nTypeScript `tsc --noEmit` reported issues after recent edits:\n\n{body}\n</new-diagnostics>"
    ))]
}

#[allow(dead_code)]
fn tsc_available() -> bool {
    std::process::Command::new("tsc")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    #[serial_test::serial]
    fn should_schedule_requires_tsconfig_and_ts_extension() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!should_schedule_passive_tsc(root, Path::new("a.ts")));
        std::fs::write(
            root.join("tsconfig.json"),
            "{\"compilerOptions\":{\"strict\":true}}\n",
        )
        .unwrap();
        assert!(should_schedule_passive_tsc(root, Path::new("src/index.ts")));
        assert!(should_schedule_passive_tsc(root, Path::new("src/App.tsx")));
        assert!(!should_schedule_passive_tsc(root, Path::new("src/x.js")));
    }

    #[tokio::test]
    async fn pending_not_consumed_when_tool_results_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("tsconfig.json"), "{\"compilerOptions\":{}}\n").unwrap();
        let pending = AtomicBool::new(true);
        let msgs = take_passive_tsc_messages(&pending, root, false).await;
        assert!(msgs.is_empty());
        assert!(pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn injects_on_type_error_when_tsc_available() {
        if !tsc_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["*.ts"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("bad.ts"), "const x: string = 42;\n").unwrap();

        let pending = AtomicBool::new(true);
        let msgs = take_passive_tsc_messages(&pending, root, true).await;
        assert_eq!(msgs.len(), 1);
        let c = msgs[0]["content"].as_str().unwrap();
        assert!(c.contains("<new-diagnostics>"), "{c}");
        assert!(c.contains("error TS") || c.contains("TypeScript"), "{c}");
        assert!(!pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn no_message_when_types_clean() {
        if !tsc_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true,"noEmit":true},"include":["*.ts"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("ok.ts"), "const x: string = 'hi';\n").unwrap();

        let pending = AtomicBool::new(true);
        let msgs = take_passive_tsc_messages(&pending, root, true).await;
        assert!(msgs.is_empty());
        assert!(!pending.load(Ordering::SeqCst));
    }

    #[test]
    #[serial_test::serial]
    fn disabled_via_env_skips_schedule() {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("ASTRA_PASSIVE_TSC_CHECK");
                }
            }
        }
        unsafe {
            std::env::set_var("ASTRA_PASSIVE_TSC_CHECK", "0");
        }
        let _c = Clear;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        assert!(!should_schedule_passive_tsc(root, Path::new("a.ts")));
    }
}
