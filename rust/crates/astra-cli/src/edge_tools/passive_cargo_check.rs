//! Passive `cargo check` after Rust source edits: run once before the next LLM turn
//! that carries `tool_results`, then inject a user message (claudecode-style
//! `<new-diagnostics>`) when there are errors or the check fails.
//!
//! Kill switch: `ASTRA_PASSIVE_CARGO_CHECK=0|false|off`
//! Timeout: `ASTRA_PASSIVE_CARGO_TIMEOUT_SECS` (default 45, max 300)

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::timeout;

use super::build_test;

/// Max chars for injected diagnostic text (keeps `/chat` payload bounded).
const MAX_PASSIVE_DIAG_CHARS: usize = 12_000;

fn passive_cargo_check_enabled() -> bool {
    match std::env::var("ASTRA_PASSIVE_CARGO_CHECK") {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "off")
        }
        Err(_) => true,
    }
}

fn passive_cargo_timeout() -> Duration {
    let secs = std::env::var("ASTRA_PASSIVE_CARGO_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(45);
    Duration::from_secs(secs.clamp(1, 300))
}

/// Whether a successful disk write to `edited_path` should schedule a passive check.
#[must_use]
pub(crate) fn should_schedule_passive_cargo(project_root: &Path, edited_path: &Path) -> bool {
    if !passive_cargo_check_enabled() {
        return false;
    }
    if edited_path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return false;
    }
    project_root.join("Cargo.toml").is_file()
}

fn truncate_diag_body(s: &str) -> String {
    if s.chars().count() <= MAX_PASSIVE_DIAG_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_PASSIVE_DIAG_CHARS).collect();
    format!("{head}\n\n[truncated passive cargo diagnostics at {MAX_PASSIVE_DIAG_CHARS} chars]")
}

/// Drain pending flag, run `cargo check` if appropriate, return messages to append to `payload["messages"]`.
pub(crate) async fn take_passive_cargo_messages(
    pending: &AtomicBool,
    project_root: &Path,
    tool_results_nonempty: bool,
) -> Vec<Value> {
    if !passive_cargo_check_enabled() || !tool_results_nonempty {
        return Vec::new();
    }
    if !pending.load(Ordering::SeqCst) {
        return Vec::new();
    }
    if !project_root.join("Cargo.toml").is_file() {
        pending.store(false, Ordering::SeqCst);
        return Vec::new();
    }
    pending.store(false, Ordering::SeqCst);

    let run = async {
        Command::new("cargo")
            .args(["check", "--message-format=short"])
            .current_dir(project_root)
            .kill_on_drop(true)
            .output()
            .await
    };

    let output = match timeout(passive_cargo_timeout(), run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return vec![diagnostic_message(format!(
                "Passive `cargo check` could not run: {e}"
            ))];
        }
        Err(_) => {
            return vec![diagnostic_message(format!(
                "Passive `cargo check` timed out after {:?}",
                passive_cargo_timeout()
            ))];
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");
    let code = output.status.code();
    let mut result = build_test::parse_build_test_output(&combined, code);
    if !result.error_locations.is_empty() {
        result.enrich_with_scope(project_root);
    }

    let exit_fail = !matches!(code, Some(0));
    let has_model_issues = !result.passed || result.error_count > 0;
    if !exit_fail && !has_model_issues {
        return Vec::new();
    }

    let body = truncate_diag_body(&result.to_enhanced_output(&combined));
    let intro = if exit_fail && result.error_count == 0 {
        "Workspace `cargo check` failed (non-zero exit) after recent Rust edits."
    } else {
        "Workspace `cargo check` reported issues after recent Rust edits."
    };
    vec![diagnostic_message(format!(
        "<new-diagnostics>\n{intro}\n\n{body}\n</new-diagnostics>"
    ))]
}

fn diagnostic_message(content: String) -> Value {
    json!({
        "role": "user",
        "content": content,
        "attachment_metadata": {
            "kind": "passive_workspace_diagnostics",
            "source": "cargo_check",
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_tools::ToolExecutor;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    #[test]
    #[serial_test::serial]
    fn should_schedule_requires_rs_and_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!should_schedule_passive_cargo(
            root,
            Path::new("src/lib.rs")
        ));
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert!(should_schedule_passive_cargo(
            root,
            Path::new("src/main.rs")
        ));
        assert!(!should_schedule_passive_cargo(root, Path::new("src/x.go")));
    }

    #[tokio::test]
    async fn pending_not_consumed_when_tool_results_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"passive_t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let pending = AtomicBool::new(true);
        let msgs = take_passive_cargo_messages(&pending, root, false).await;
        assert!(msgs.is_empty());
        assert!(pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn injects_on_compile_error_after_tool_round() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"passive_t2\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() { let x: String = 42; }\n",
        )
        .unwrap();

        let pending = AtomicBool::new(true);
        let msgs = take_passive_cargo_messages(&pending, root, true).await;
        assert_eq!(msgs.len(), 1);
        let content = msgs[0]["content"].as_str().unwrap();
        assert!(content.contains("<new-diagnostics>"), "content={content:?}");
        assert!(
            content.contains("error") || content.contains("mismatch"),
            "expected rustc hint, content={content:?}"
        );
        assert!(!pending.load(Ordering::SeqCst));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn tool_executor_write_file_triggers_passive_flush() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"passive_t4\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let exe = ToolExecutor::new(root);
        let _ = exe
            .execute("read_file", &json!({"path": "src/main.rs"}))
            .await;
        let r = exe
            .execute(
                "write_file",
                &json!({"path": "src/main.rs", "content": "fn main() { let _: () = 1; }\n"}),
            )
            .await;
        assert!(r.contains("\"success\":true"), "write_file: {r}");
        let msgs = exe
            .take_passive_workspace_diagnostic_messages(root, true)
            .await;
        assert_eq!(msgs.len(), 1);
        let c = msgs[0]["content"].as_str().unwrap();
        assert!(c.contains("<new-diagnostics>"), "{c}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn no_message_when_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"passive_t3\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let pending = AtomicBool::new(true);
        let msgs = take_passive_cargo_messages(&pending, root, true).await;
        assert!(msgs.is_empty());
        assert!(!pending.load(Ordering::SeqCst));
    }

    #[test]
    #[serial_test::serial]
    fn disabled_via_env_skips_schedule() {
        struct ClearPassiveEnv;
        impl Drop for ClearPassiveEnv {
            fn drop(&mut self) {
                // SAFETY: `serial` test; no concurrent readers of this env var in other threads.
                unsafe {
                    std::env::remove_var("ASTRA_PASSIVE_CARGO_CHECK");
                }
            }
        }
        // SAFETY: `serial` test; no concurrent env access in other threads.
        unsafe {
            std::env::set_var("ASTRA_PASSIVE_CARGO_CHECK", "0");
        }
        let _clear = ClearPassiveEnv;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert!(!should_schedule_passive_cargo(root, Path::new("a.rs")));
    }
}
