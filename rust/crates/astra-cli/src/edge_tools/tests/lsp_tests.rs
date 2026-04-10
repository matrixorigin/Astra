use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn fake_lsp_server_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-rust-analyzer");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import os
import sys

def read_frame():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode("utf-8").split(":", 1)
        headers[key.strip().lower()] = value.strip()
    length = int(headers["content-length"])
    body = sys.stdin.buffer.read(length)
    return json.loads(body.decode("utf-8"))

def write_frame(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

LOG_PATH = os.environ.get("FAKE_LSP_LOG")

def log_event(name):
    if not LOG_PATH:
        return
    with open(LOG_PATH, "a", encoding="utf-8") as fh:
        fh.write(name + "\n")

while True:
    message = read_frame()
    if message is None:
        break
    method = message.get("method")
    msg_id = message.get("id")
    if method == "initialize":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "capabilities": {
                    "definitionProvider": True,
                    "documentSymbolProvider": True,
                    "codeActionProvider": True
                }
            }
        })
    elif method == "textDocument/documentSymbol":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "hello_from_lsp",
                "kind": 12,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 18}
                },
                "selectionRange": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 17}
                },
                "detail": uri
            }]
        })
    elif method == "textDocument/definition":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 17}
                }
            }]
        })
    elif method == "textDocument/rename":
        uri = message["params"]["textDocument"]["uri"]
        new_name = message["params"]["newName"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "changes": {
                    uri: [{
                        "range": {
                            "start": {"line": 0, "character": 7},
                            "end": {"line": 0, "character": 21}
                        },
                        "newText": new_name
                    }]
                }
            }
        })
    elif method == "textDocument/codeAction":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "title": "Apply fake quick fix",
                "kind": "quickfix",
                "diagnostics": message["params"]["context"]["diagnostics"],
                "edit": {
                    "changes": {
                        uri: [{
                            "range": {
                                "start": {"line": 0, "character": 7},
                                "end": {"line": 0, "character": 21}
                            },
                            "newText": "hello_from_fix"
                        }]
                    }
                }
            }, {
                "title": "Apply second fake fix",
                "kind": "quickfix",
                "diagnostics": message["params"]["context"]["diagnostics"],
                "edit": {
                    "changes": {
                        uri: [{
                            "range": {
                                "start": {"line": 0, "character": 7},
                                "end": {"line": 0, "character": 21}
                            },
                            "newText": "hello_from_second_fix"
                        }]
                    }
                }
            }]
        })
    elif method in ("textDocument/didOpen", "textDocument/didChange", "textDocument/didSave"):
        log_event(method)
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 7},
                        "end": {"line": 0, "character": 21}
                    },
                    "severity": 2,
                    "message": "fake LSP diagnostic"
                }]
            }
        })
    elif msg_id is not None:
        write_frame({"jsonrpc": "2.0", "id": msg_id, "result": None})
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

#[cfg(unix)]
struct EnvGuard {
    key: &'static str,
    old: Option<String>,
}

#[cfg(unix)]
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let old = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, old }
    }
}

#[cfg(unix)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

#[cfg(unix)]
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.lines().filter(|line| *line == needle).count()
}

// ─── lsp tests ────────────────────────────────────────────────────────────────

#[test]
fn lsp_missing_operation_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({}));
    assert!(result.contains("error"));
    assert!(result.contains("operation"));
}

#[test]
fn lsp_invalid_operation_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "invalid_op"}));
    assert!(result.contains("error"));
    assert!(result.contains("Unknown operation"));
}

#[test]
fn lsp_diagnostics_returns_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "diagnostics"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["capabilities"]["goto_definition"].as_bool().unwrap());
    assert!(parsed["capabilities"]["find_references"].as_bool().unwrap());
    assert!(parsed["capabilities"]["code_actions"].as_bool().unwrap());
    assert!(
        parsed["supported_languages"]["active_lsp"]
            .as_array()
            .is_some()
    );
    assert!(parsed["active_backends"]["rust"]["workspace_detected"].is_boolean());
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_diagnostics_returns_file_snapshot_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "diagnostics",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("publishDiagnostics"));
    assert_eq!(
        parsed["result"]["diagnostics"][0]["message"].as_str(),
        Some("fake LSP diagnostic")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_actions_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "code_actions",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/codeAction"));
    assert_eq!(
        parsed["result"][0]["title"].as_str(),
        Some("Apply fake quick fix")
    );
    assert_eq!(
        parsed["result"][0]["diagnostics"][0]["message"].as_str(),
        Some("fake LSP diagnostic")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_actions_apply_selected_workspace_edit_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    std::fs::write(&file_path, "pub fn hello_from_lsp() {}\n").unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "code_actions",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8,
        "action_index": 1,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert!(
        std::fs::read_to_string(file_path)
            .unwrap()
            .contains("hello_from_second_fix")
    );
}

#[test]
fn lsp_goto_definition_requires_symbol_or_position() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "goto_definition"}));
    assert!(result.contains("error"));
    assert!(result.contains("symbol"));
}

#[test]
fn lsp_find_references_requires_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "find_references"}));
    assert!(result.contains("error"));
    assert!(result.contains("symbol"));
}

#[test]
fn lsp_document_symbols_requires_file() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "document_symbols"}));
    assert!(result.contains("error"));
    assert!(result.contains("file"));
}

#[test]
fn lsp_workspace_symbols_with_query() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a test file with a symbol
    let test_file = dir.path().join("test.rs");
    std::fs::write(&test_file, "fn hello_world() {}\nfn goodbye() {}").unwrap();

    // workspace_symbols should work with query
    let result = exe.lsp(&json!({
        "operation": "workspace_symbols",
        "query": "hello"
    }));
    // Should return results (format depends on symbol_search implementation)
    assert!(!result.contains("error") || result.contains("No symbols"));
}

#[test]
fn lsp_call_hierarchy_requires_file() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({"operation": "call_hierarchy"}));
    assert!(result.contains("error"));
    assert!(result.contains("file"));
}

#[test]
fn lsp_document_symbols_on_rust_file() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // Create a test Rust file
    let test_file = dir.path().join("lib.rs");
    std::fs::write(
        &test_file,
        r#"
pub fn main() {}
fn helper() {}
struct Config {}
impl Config {
    fn new() -> Self { Config {} }
}
"#,
    )
    .unwrap();

    let result = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "lib.rs"
    }));

    // Should find symbols
    assert!(!result.contains("Error:") || result.contains("main"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_document_symbols_prefers_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/documentSymbol")
    );
    assert_eq!(parsed["result"][0]["name"].as_str(), Some("hello_from_lsp"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_repeated_query_skips_redundant_resync_for_unchanged_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let log_path = dir.path().join("fake-lsp.log");
    let script = fake_lsp_server_script(dir.path());
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _log_guard = EnvGuard::set("FAKE_LSP_LOG", log_path.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let _ = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));
    let _ = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));

    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    assert_eq!(count_occurrences(&log, "textDocument/didOpen"), 1);
    assert_eq!(count_occurrences(&log, "textDocument/didSave"), 1);
    assert_eq!(count_occurrences(&log, "textDocument/didChange"), 0);
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn write_file_syncs_lsp_once_before_followup_query() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let log_path = dir.path().join("fake-lsp.log");
    let script = fake_lsp_server_script(dir.path());
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _log_guard = EnvGuard::set("FAKE_LSP_LOG", log_path.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let write_result = exe.write_file(&json!({
        "path": "src/lib.rs",
        "content": "pub fn hello_from_lsp() {}\n"
    }));
    assert!(write_result.contains("\"success\":true"));

    let _ = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));

    let log = std::fs::read_to_string(log_path).unwrap_or_default();
    assert_eq!(count_occurrences(&log, "textDocument/didOpen"), 1);
    assert_eq!(count_occurrences(&log, "textDocument/didSave"), 1);
    assert_eq!(count_occurrences(&log, "textDocument/didChange"), 0);
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_rename_uses_real_lsp_preview_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "rename",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8,
        "new_name": "renamed_from_lsp"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/rename"));
    assert_eq!(
        parsed["result"]["changes"]
            .as_object()
            .and_then(|changes| changes.values().next())
            .and_then(|edits| edits.as_array())
            .and_then(|edits| edits.first())
            .and_then(|edit| edit["newText"].as_str()),
        Some("renamed_from_lsp")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_rename_applies_real_lsp_workspace_edit_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    std::fs::write(&file_path, "pub fn hello_from_lsp() {}\n").unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "rename",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8,
        "new_name": "renamed_from_lsp",
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert!(
        std::fs::read_to_string(file_path)
            .unwrap()
            .contains("renamed_from_lsp")
    );
}

#[test]
fn lsp_rename_falls_back_to_rename_symbol() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn hello() { hello(); }\n").unwrap();
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "rename",
        "symbol": "hello",
        "new_name": "goodbye",
        "dry_run": true
    }));

    assert!(result.contains("Rename preview"));
    assert!(result.contains("hello"));
    assert!(result.contains("goodbye"));
}
