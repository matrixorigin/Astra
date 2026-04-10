use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn fake_lsp_server_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("fake-rust-analyzer");
    std::fs::write(
        &script,
        r##"#!/usr/bin/env python3
import json
import os
import sys
import urllib.parse

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
CAPTURE_PATH = os.environ.get("FAKE_LSP_CAPTURE")
REQUEST_CONFIG = os.environ.get("FAKE_LSP_REQUEST_CONFIG") == "1"
PULL_DIAGNOSTICS_MODE = os.environ.get("FAKE_LSP_PULL_DIAGNOSTICS", "full")
CONFIG_REQUEST_ID = 9001

def log_event(name):
    if not LOG_PATH:
        return
    with open(LOG_PATH, "a", encoding="utf-8") as fh:
        fh.write(name + "\n")

def fake_diagnostics():
    return [{
        "range": {
            "start": {"line": 0, "character": 7},
            "end": {"line": 0, "character": 21}
        },
        "severity": 2,
        "message": "fake LSP diagnostic"
    }]

def capture(kind, payload):
    if not CAPTURE_PATH:
        return
    with open(CAPTURE_PATH, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"kind": kind, "payload": payload}) + "\n")

while True:
    message = read_frame()
    if message is None:
        break
    method = message.get("method")
    msg_id = message.get("id")
    if msg_id == CONFIG_REQUEST_ID and "result" in message:
        capture("workspace/configuration", message["result"])
    if method == "initialize":
        capture("initialize", message.get("params"))
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "capabilities": {
                    "declarationProvider": True,
                    "definitionProvider": True,
                    "typeDefinitionProvider": True,
                    "typeHierarchyProvider": True,
                    "documentSymbolProvider": True,
                    "documentFormattingProvider": True,
                    "documentRangeFormattingProvider": True,
                    "documentOnTypeFormattingProvider": {
                        "firstTriggerCharacter": ";",
                        "moreTriggerCharacter": ["}"]
                    },
                    "renameProvider": {"prepareProvider": True},
                    "codeActionProvider": {"resolveProvider": True},
                    "completionProvider": {"resolveProvider": True},
                    "documentHighlightProvider": True,
                    "documentLinkProvider": {"resolveProvider": False},
                    "inlayHintProvider": True,
                    "foldingRangeProvider": True,
                    "colorProvider": True,
                    "semanticTokensProvider": {
                        "legend": {
                            "tokenTypes": ["type"],
                            "tokenModifiers": []
                        },
                        "full": True
                    },
                    "codeLensProvider": {"resolveProvider": True},
                    "selectionRangeProvider": True,
                    "linkedEditingRangeProvider": True,
                    "signatureHelpProvider": {
                        "triggerCharacters": ["("]
                    },
                    "diagnosticProvider": {
                        "interFileDependencies": False,
                        "workspaceDiagnostics": False
                    } if PULL_DIAGNOSTICS_MODE != "unsupported" else None
                }
            }
        })
    elif method == "initialized":
        if REQUEST_CONFIG:
            write_frame({
                "jsonrpc": "2.0",
                "id": CONFIG_REQUEST_ID,
                "method": "workspace/configuration",
                "params": {
                    "items": [{"section": "rust-analyzer"}]
                }
            })
    elif method == "workspace/didChangeConfiguration":
        capture("workspace/didChangeConfiguration", message.get("params"))
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
    elif method == "textDocument/declaration":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 4},
                    "end": {"line": 0, "character": 6}
                }
            }]
        })
    elif method == "textDocument/typeDefinition":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 2}
                }
            }]
        })
    elif method == "textDocument/implementation":
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
    elif method == "textDocument/prepareTypeHierarchy":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "HelloType",
                "kind": 5,
                "uri": uri,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 18}
                },
                "selectionRange": {
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 12}
                }
            }]
        })
    elif method == "typeHierarchy/supertypes":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "Greeting",
                "kind": 11,
                "uri": message["params"]["item"]["uri"],
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 8}
                },
                "selectionRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 8}
                }
            }]
        })
    elif method == "typeHierarchy/subtypes":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "name": "FriendlyGreeting",
                "kind": 5,
                "uri": message["params"]["item"]["uri"],
                "range": {
                    "start": {"line": 1, "character": 0},
                    "end": {"line": 1, "character": 16}
                },
                "selectionRange": {
                    "start": {"line": 1, "character": 0},
                    "end": {"line": 1, "character": 16}
                }
            }]
        })
    elif method == "textDocument/prepareRename":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "range": {
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 21}
                },
                "placeholder": "hello_from_lsp"
            }
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
            }, {
                "title": "Run fake command fix",
                "kind": "quickfix",
                "diagnostics": message["params"]["context"]["diagnostics"],
                "command": {
                    "title": "Run fake command fix",
                    "command": "fake.applyCommandFix",
                    "arguments": [uri]
                }
            }, {
                "title": "Resolve fake edit fix",
                "kind": "quickfix",
                "diagnostics": message["params"]["context"]["diagnostics"],
                "data": {
                    "resolve_kind": "edit",
                    "uri": uri
                }
            }, {
                "title": "Apply snippet fake fix",
                "kind": "quickfix",
                "diagnostics": message["params"]["context"]["diagnostics"],
                "edit": {
                    "changes": {
                        uri: [{
                            "range": {
                                "start": {"line": 0, "character": 7},
                                "end": {"line": 0, "character": 21}
                            },
                            "newText": "hello_${1:snippet}$0",
                            "insertTextFormat": 2
                        }]
                    }
                }
            }]
        })
    elif method == "workspace/executeCommand":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "executedCommand": message["params"]["command"],
                "arguments": message["params"].get("arguments", [])
            }
        })
    elif method == "codeAction/resolve":
        action = message["params"]
        uri = action["data"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                **action,
                "edit": {
                    "changes": {
                        uri: [{
                            "range": {
                                "start": {"line": 0, "character": 7},
                                "end": {"line": 0, "character": 21}
                            },
                            "newText": "hello_from_resolved_fix"
                        }]
                    }
                }
            }
        })
    elif method == "textDocument/completion":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "isIncomplete": False,
                "items": [{
                    "label": "hello_completion",
                    "kind": 3,
                    "detail": "fake completion"
                }, {
                    "label": "resolved_completion",
                    "kind": 3,
                    "data": {
                        "resolve_kind": "completion"
                    }
                }]
            }
        })
    elif method == "completionItem/resolve":
        item = message["params"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                **item,
                "detail": "resolved completion detail",
                "documentation": "resolved completion docs",
                "textEdit": {
                    "range": {
                        "start": {"line": 0, "character": 7},
                        "end": {"line": 0, "character": 21}
                    },
                    "newText": "resolved_completion(${1:value})$0"
                },
                "additionalTextEdits": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 0}
                    },
                    "newText": "// resolved completion\n"
                }]
            }
        })
    elif method == "textDocument/signatureHelp":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "signatures": [{
                    "label": "hello_from_lsp(name: &str)",
                    "documentation": "fake signature help"
                }],
                "activeSignature": 0,
                "activeParameter": 0
            }
        })
    elif method == "textDocument/diagnostic":
        result = None
        if PULL_DIAGNOSTICS_MODE == "full":
            result = {
                "kind": "full",
                "resultId": "fake-diagnostics-v1",
                "items": fake_diagnostics()
            }
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": result
        })
    elif method == "textDocument/documentHighlight":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 21}
                },
                "kind": 1
            }]
        })
    elif method == "textDocument/documentLink":
        uri = message["params"]["textDocument"]["uri"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                },
                "target": uri + "?hello",
                "tooltip": "fake document link"
            }]
        })
    elif method == "textDocument/inlayHint":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "position": {"line": 0, "character": 3},
                "label": ": ()",
                "kind": 2,
                "paddingLeft": True
            }]
        })
    elif method == "textDocument/foldingRange":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "startLine": 0,
                "endLine": 2,
                "kind": "region"
            }]
        })
    elif method == "textDocument/documentColor":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 20},
                    "end": {"line": 0, "character": 27}
                },
                "color": {
                    "red": 1.0,
                    "green": 0.0,
                    "blue": 0.0,
                    "alpha": 1.0
                }
            }]
        })
    elif method == "textDocument/colorPresentation":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "label": "#ff0000",
                "textEdit": {
                    "range": message["params"]["range"],
                    "newText": "#ff0000"
                }
            }]
        })
    elif method == "textDocument/semanticTokens/full":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "data": [0, 0, 5, 0, 0]
            }
        })
    elif method == "textDocument/codeLens":
        if os.environ.get("FAKE_LSP_RUST_ANALYZER_CODE_LENS") == "1":
            uri = message["params"]["textDocument"]["uri"]
            path = urllib.parse.urlparse(uri).path
            workspace = os.path.dirname(os.path.dirname(path))
            runnable = {
                "label": "run fake runnable",
                "location": {
                    "targetUri": uri,
                    "targetRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    },
                    "targetSelectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    }
                },
                "kind": "cargo",
                "args": {
                    "cwd": workspace,
                    "workspaceRoot": workspace,
                    "overrideCargo": None,
                    "cargoArgs": ["run", "--quiet"],
                    "executableArgs": [],
                    "environment": {}
                }
            }
            write_frame({
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": [{
                    "range": runnable["location"]["targetSelectionRange"],
                    "command": {
                        "title": "▶ Run",
                        "command": "rust-analyzer.runSingle",
                        "arguments": [runnable]
                    }
                }]
            })
        elif os.environ.get("FAKE_LSP_EMPTY_CODE_LENSES") == "1":
            write_frame({
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": []
            })
        else:
            write_frame({
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    },
                    "command": {
                        "title": "1 reference",
                        "command": "fake.references"
                    }
                }, {
                    "range": {
                        "start": {"line": 0, "character": 4},
                        "end": {"line": 0, "character": 8}
                    },
                    "data": {
                        "resolve_kind": "code_lens"
                    }
                }]
            })
    elif method == "codeLens/resolve":
        lens = message["params"]
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                **lens,
                "command": {
                    "title": "2 references",
                    "command": "fake.resolvedReferences",
                    "arguments": ["resolved-code-lens"]
                }
            }
        })
    elif method == "experimental/runnables":
        uri = message["params"]["textDocument"]["uri"]
        path = urllib.parse.urlparse(uri).path
        workspace = os.path.dirname(os.path.dirname(path))
        runnables = []
        if os.environ.get("FAKE_LSP_MULTI_RUNNABLES") == "1":
            runnables.append({
                "label": "cargo check --workspace",
                "location": {
                    "targetUri": uri,
                    "targetRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    },
                    "targetSelectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 3}
                    }
                },
                "kind": "cargo",
                "args": {
                    "cwd": workspace,
                    "workspaceRoot": workspace,
                    "overrideCargo": None,
                    "cargoArgs": ["check", "--workspace"],
                    "executableArgs": [],
                    "environment": {}
                }
            })
        runnables.append({
            "label": "run fake runnable",
            "location": {
                "targetUri": uri,
                "targetRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                },
                "targetSelectionRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }
            },
            "kind": "cargo",
            "args": {
                "cwd": workspace,
                "workspaceRoot": workspace,
                "overrideCargo": None,
                "cargoArgs": ["run", "--quiet"],
                "executableArgs": [],
                "environment": {}
            }
        })
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": runnables
        })
    elif method == "textDocument/selectionRange":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 21}
                },
                "parent": {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 25}
                    }
                }
            }]
        })
    elif method == "textDocument/linkedEditingRange":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "ranges": [{
                    "start": {"line": 0, "character": 7},
                    "end": {"line": 0, "character": 21}
                }, {
                    "start": {"line": 0, "character": 24},
                    "end": {"line": 0, "character": 38}
                }],
                "wordPattern": "[A-Za-z_][A-Za-z0-9_]*"
            }
        })
    elif method == "textDocument/formatting":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 5}
                },
                "newText": " "
            }]
        })
    elif method == "textDocument/rangeFormatting":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 3},
                    "end": {"line": 0, "character": 5}
                },
                "newText": " "
            }]
        })
    elif method == "textDocument/onTypeFormatting":
        write_frame({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": [{
                "range": {
                    "start": {"line": 0, "character": 24},
                    "end": {"line": 0, "character": 26}
                },
                "newText": "{\n}"
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
                "diagnostics": fake_diagnostics()
            }
        })
    elif msg_id is not None:
        write_frame({"jsonrpc": "2.0", "id": msg_id, "result": None})
"##,
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

    fn unset(key: &'static str) -> Self {
        let old = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
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

#[cfg(unix)]
fn read_capture_entries(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(unix)]
fn wait_for_capture_entries(path: &std::path::Path, min_entries: usize) -> Vec<Value> {
    for _ in 0..20 {
        let entries = read_capture_entries(path);
        if entries.len() >= min_entries {
            return entries;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    read_capture_entries(path)
}

#[cfg(unix)]
fn real_rust_analyzer_available() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
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
    let exe = ToolExecutor::new(dir.path());
    let result = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));
    serde_json::from_str::<Value>(&result)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("backend")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("lsp")
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
    assert!(parsed["capabilities"]["completions"].as_bool().unwrap());
    assert!(parsed["capabilities"]["signature_help"].as_bool().unwrap());
    assert!(parsed["capabilities"]["declaration"].as_bool().unwrap());
    assert!(
        parsed["capabilities"]["document_highlight"]
            .as_bool()
            .unwrap()
    );
    assert!(parsed["capabilities"]["document_links"].as_bool().unwrap());
    assert!(parsed["capabilities"]["inlay_hints"].as_bool().unwrap());
    assert!(parsed["capabilities"]["folding_ranges"].as_bool().unwrap());
    assert!(parsed["capabilities"]["document_colors"].as_bool().unwrap());
    assert!(
        parsed["capabilities"]["color_presentations"]
            .as_bool()
            .unwrap()
    );
    assert!(parsed["capabilities"]["semantic_tokens"].as_bool().unwrap());
    assert!(parsed["capabilities"]["code_lenses"].as_bool().unwrap());
    assert!(
        parsed["capabilities"]["selection_ranges"]
            .as_bool()
            .unwrap()
    );
    assert!(
        parsed["capabilities"]["linked_editing_range"]
            .as_bool()
            .unwrap()
    );
    assert!(parsed["capabilities"]["format_document"].as_bool().unwrap());
    assert!(parsed["capabilities"]["format_range"].as_bool().unwrap());
    assert!(parsed["capabilities"]["format_on_type"].as_bool().unwrap());
    assert!(parsed["capabilities"]["type_definition"].as_bool().unwrap());
    assert!(parsed["capabilities"]["implementation"].as_bool().unwrap());
    assert!(parsed["capabilities"]["supertypes"].as_bool().unwrap());
    assert!(parsed["capabilities"]["subtypes"].as_bool().unwrap());
    assert!(parsed["capabilities"]["prepare_rename"].as_bool().unwrap());
    assert!(
        parsed["recommended_operations"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("code_actions")))
    );
    assert!(
        parsed["recommended_operations"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("diagnostics")))
    );
    assert!(
        parsed["advanced_editor_operations"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("semantic_tokens")))
    );
    assert!(
        parsed["advanced_editor_operations"]
            .as_array()
            .is_some_and(|ops| ops.iter().any(|op| op.as_str() == Some("document_colors")))
    );
    assert!(
        parsed["supported_languages"]["active_lsp"]
            .as_array()
            .is_some()
    );
    assert!(parsed["active_backends"]["rust"]["workspace_detected"].is_boolean());
    assert!(parsed["active_backends"]["rust"]["enabled"].is_boolean());
    assert!(parsed["active_backends"]["rust"]["command_available"].is_boolean());
    assert!(parsed["active_backends"]["rust"]["session_started"].is_boolean());
    assert!(parsed["active_backends"]["rust"]["session_state"].is_string());
    assert!(parsed["active_backends"]["rust"]["last_start_error"].is_null());
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_diagnostics_reports_last_start_error_for_missing_rust_backend() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let rust_file = dir.path().join("src/lib.rs");
    std::fs::write(&rust_file, "pub fn hello_from_lsp() {}\n").unwrap();

    let _enabled_guard = EnvGuard::set("ASTRA_LSP_RUST", "1");
    let _cmd_guard = EnvGuard::set(
        "ASTRA_RUST_ANALYZER_CMD",
        "definitely-missing-rust-analyzer",
    );

    let exe = ToolExecutor::new(dir.path());
    let request = serde_json::json!({
        "operation": "document_links",
        "file": rust_file.to_string_lossy(),
    });
    let failure = exe.lsp(&request);
    assert!(failure.contains("failed to start rust-analyzer LSP session"));

    let status = exe.lsp(&serde_json::json!({"operation": "diagnostics"}));
    let parsed: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(
        parsed["active_backends"]["rust"]["session_state"].as_str(),
        Some("command_missing")
    );
    assert_eq!(
        parsed["active_backends"]["rust"]["command_available"].as_bool(),
        Some(false)
    );
    let last_error = parsed["active_backends"]["rust"]["last_start_error"]
        .as_str()
        .unwrap_or("");
    assert!(last_error.contains("failed to start rust-analyzer LSP session"));
    assert!(last_error.contains("definitely-missing-rust-analyzer"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_rust_session_sends_rust_analyzer_init_and_configuration() {
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
    let capture = dir.path().join("fake-lsp-capture.jsonl");
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _capture_guard = EnvGuard::set("FAKE_LSP_CAPTURE", capture.to_str().unwrap());
    let _request_config_guard = EnvGuard::set("FAKE_LSP_REQUEST_CONFIG", "1");
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["backend"].as_str(), Some("lsp"));

    let entries = wait_for_capture_entries(&capture, 3);
    let initialize = entries
        .iter()
        .find(|entry| entry["kind"].as_str() == Some("initialize"))
        .unwrap();
    assert_eq!(
        initialize["payload"]["initializationOptions"]["lens"]["enable"].as_bool(),
        Some(true)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["experimental"]["snippetTextEdit"].as_bool(),
        Some(true)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["experimental"]["commands"]["commands"][0].as_str(),
        Some("rust-analyzer.runSingle")
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["experimental"]["commands"]["commands"][2].as_str(),
        Some("rust-analyzer.showReferences")
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["experimental"]["hoverActions"].as_bool(),
        Some(true)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["codeAction"]
            ["codeActionLiteralSupport"]["codeActionKind"]["valueSet"][0]
            .as_str(),
        Some("quickfix")
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["completion"]["completionItem"]
            ["snippetSupport"]
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["hover"]["contentFormat"][0].as_str(),
        Some("markdown")
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["signatureHelp"]
            ["signatureInformation"]["parameterInformation"]["labelOffsetSupport"]
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["diagnostic"]["dynamicRegistration"]
            .as_bool(),
        Some(false)
    );
    assert_eq!(
        initialize["payload"]["capabilities"]["textDocument"]["completion"]["completionItem"]
            ["resolveSupport"]["properties"][2]
            .as_str(),
        Some("additionalTextEdits")
    );

    let did_change_configuration = entries
        .iter()
        .find(|entry| entry["kind"].as_str() == Some("workspace/didChangeConfiguration"))
        .unwrap();
    assert_eq!(
        did_change_configuration["payload"]["settings"]["rust-analyzer"]["lens"]["run"]["enable"]
            .as_bool(),
        Some(true)
    );

    let workspace_configuration = entries
        .iter()
        .find(|entry| entry["kind"].as_str() == Some("workspace/configuration"))
        .unwrap();
    assert_eq!(
        workspace_configuration["payload"][0]["lens"]["debug"]["enable"].as_bool(),
        Some(true)
    );
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
    assert_eq!(parsed["method"].as_str(), Some("textDocument/diagnostic"));
    assert_eq!(
        parsed["result"]["source_method"].as_str(),
        Some("textDocument/diagnostic")
    );
    assert_eq!(
        parsed["result"]["diagnostics"][0]["message"].as_str(),
        Some("fake LSP diagnostic")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_diagnostics_fall_back_to_publish_snapshot_when_pull_returns_null() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp(name: &str) {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _pull_guard = EnvGuard::set("FAKE_LSP_PULL_DIAGNOSTICS", "null");
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "diagnostics",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("publishDiagnostics"));
    assert_eq!(
        parsed["result"]["source_method"].as_str(),
        Some("publishDiagnostics")
    );
    assert!(
        parsed["result"]["pull_diagnostics_error"]
            .as_str()
            .is_some_and(|s| s.contains("no usable diagnostic items"))
    );
    assert_eq!(
        parsed["result"]["diagnostics"][0]["message"].as_str(),
        Some("fake LSP diagnostic")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_surfaces_rust_lsp_startup_errors_for_supported_workspaces() {
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
    let _guard = EnvGuard::set(
        "ASTRA_RUST_ANALYZER_CMD",
        "/definitely/missing/rust-analyzer",
    );
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "document_symbols",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let error = parsed["error"].as_str().unwrap();

    assert!(error.contains("failed to start rust-analyzer LSP session"));
    assert!(error.contains("/definitely/missing/rust-analyzer"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_implementation_uses_real_lsp_when_available() {
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
        "operation": "implementation",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/implementation")
    );
    assert_eq!(
        parsed["result"][0]["range"]["start"]["line"].as_u64(),
        Some(0)
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_prepare_rename_uses_real_lsp_when_available() {
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
        "operation": "prepare_rename",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/prepareRename")
    );
    assert_eq!(
        parsed["result"]["placeholder"].as_str(),
        Some("hello_from_lsp")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_declaration_uses_real_lsp_when_available() {
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
        "operation": "declaration",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/declaration"));
    assert_eq!(
        parsed["result"][0]["range"]["start"]["character"].as_u64(),
        Some(4)
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_type_definition_uses_real_lsp_when_available() {
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
        "operation": "type_definition",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/typeDefinition")
    );
    assert_eq!(
        parsed["result"][0]["range"]["end"]["character"].as_u64(),
        Some(2)
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_document_highlight_uses_real_lsp_when_available() {
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
        "operation": "document_highlight",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/documentHighlight")
    );
    assert_eq!(parsed["result"][0]["kind"].as_u64(), Some(1));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_document_links_uses_real_lsp_when_available() {
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
        "operation": "document_links",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/documentLink"));
    assert_eq!(
        parsed["result"][0]["tooltip"].as_str(),
        Some("fake document link")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_inlay_hints_use_real_lsp_when_available() {
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
        "operation": "inlay_hints",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/inlayHint"));
    assert_eq!(parsed["result"][0]["label"].as_str(), Some(": ()"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_folding_ranges_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "folding_ranges",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/foldingRange"));
    assert_eq!(parsed["result"][0]["endLine"].as_u64(), Some(2));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_document_colors_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "const RED: &str = \"#ff0000\";\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "document_colors",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/documentColor")
    );
    assert_eq!(parsed["result"][0]["color"]["red"].as_f64(), Some(1.0));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_color_presentations_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "const RED: &str = \"#ff0000\";\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "color_presentations",
        "file": "src/lib.rs",
        "line": 1,
        "column": 22
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/colorPresentation")
    );
    assert_eq!(parsed["result"][0]["label"].as_str(), Some("#ff0000"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_semantic_tokens_use_real_lsp_when_available() {
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
        "operation": "semantic_tokens",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/semanticTokens/full")
    );
    assert_eq!(parsed["result"]["data"][0].as_u64(), Some(0));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_supertypes_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "trait Greeting {}\nstruct HelloType;\nimpl Greeting for HelloType {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "supertypes",
        "file": "src/lib.rs",
        "line": 2,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("typeHierarchy/supertypes"));
    assert_eq!(parsed["result"][0]["name"].as_str(), Some("Greeting"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_subtypes_use_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "trait Greeting {}\nstruct FriendlyGreeting;\nimpl Greeting for FriendlyGreeting {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "subtypes",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("typeHierarchy/subtypes"));
    assert_eq!(
        parsed["result"][0]["name"].as_str(),
        Some("FriendlyGreeting")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_lenses_use_real_lsp_when_available() {
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
        "operation": "code_lenses",
        "file": "src/lib.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/codeLens"));
    assert_eq!(
        parsed["result"][0]["command"]["title"].as_str(),
        Some("1 reference")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_lenses_resolve_selected_item_when_item_index_provided() {
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
        "operation": "code_lenses",
        "file": "src/lib.rs",
        "item_index": 1
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("codeLens/resolve"));
    assert_eq!(parsed["selected_index"].as_u64(), Some(1));
    assert_eq!(
        parsed["result"]["command"]["title"].as_str(),
        Some("2 references")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_lenses_execute_selected_item_when_dry_run_false() {
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
        "operation": "code_lenses",
        "file": "src/lib.rs",
        "item_index": 1,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["executed"].as_bool(), Some(true));
    assert_eq!(parsed["method"].as_str(), Some("workspace/executeCommand"));
    assert_eq!(parsed["command"].as_str(), Some("fake.resolvedReferences"));
    assert_eq!(
        parsed["result"]["arguments"][0].as_str(),
        Some("resolved-code-lens")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_lenses_fall_back_to_rust_analyzer_runnables_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _lens_guard = EnvGuard::set("FAKE_LSP_EMPTY_CODE_LENSES", "1");
    let _multi_guard = EnvGuard::set("FAKE_LSP_MULTI_RUNNABLES", "1");
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "code_lenses",
        "file": "src/main.rs"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("experimental/runnables"));
    assert_eq!(
        parsed["fallback_from"].as_str(),
        Some("textDocument/codeLens")
    );
    assert_eq!(
        parsed["result"][0]["command"]["title"].as_str(),
        Some("run fake runnable")
    );
    assert_eq!(
        parsed["result"][0]["data"]["preferred"].as_bool(),
        Some(true)
    );
    assert_eq!(
        parsed["result"][0]["data"]["command_preview"].as_str(),
        Some("cargo run --quiet")
    );
    assert_eq!(
        parsed["result"][1]["command"]["title"].as_str(),
        Some("cargo check --workspace")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_lenses_execute_rust_analyzer_runnable_fallback_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { println!(\"fake-runnable-output\"); }\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _lens_guard = EnvGuard::set("FAKE_LSP_EMPTY_CODE_LENSES", "1");
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "code_lenses",
        "file": "src/main.rs",
        "item_index": 0,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["method"].as_str(), Some("experimental/runnables"));
    assert_eq!(
        parsed["fallback_from"].as_str(),
        Some("textDocument/codeLens")
    );
    assert_eq!(parsed["source"].as_str(), Some("rust-analyzer-runnables"));
    assert_eq!(parsed["executed"].as_bool(), Some(true));
    assert_eq!(parsed["command"].as_str(), Some("cargo"));
    assert_eq!(parsed["cwd"].as_str(), Some(dir.path().to_str().unwrap()));
    let expected_command_line = format!(
        "cd {} && env {} {} {}",
        crate::edge_tools::shell::shell_escape(dir.path().to_str().unwrap()),
        crate::edge_tools::shell::shell_escape("cargo"),
        crate::edge_tools::shell::shell_escape("run"),
        crate::edge_tools::shell::shell_escape("--quiet")
    );
    assert_eq!(
        parsed["command_line"].as_str(),
        Some(expected_command_line.as_str())
    );
    assert!(
        parsed["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("fake-runnable-output"))
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_selection_ranges_uses_real_lsp_when_available() {
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
        "operation": "selection_ranges",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/selectionRange")
    );
    assert_eq!(
        parsed["result"][0]["parent"]["range"]["end"]["character"].as_u64(),
        Some(25)
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_linked_editing_range_uses_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp(hello_from_lsp: i32) {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "linked_editing_range",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/linkedEditingRange")
    );
    assert_eq!(
        parsed["result"]["ranges"][1]["end"]["character"].as_u64(),
        Some(38)
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_document_uses_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub  fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "format_document",
        "file": "src/lib.rs",
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/formatting"));
    assert_eq!(parsed["result"][0]["newText"].as_str(), Some(" "));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_document_applies_text_edits_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    std::fs::write(&file_path, "pub  fn hello_from_lsp() {}\n").unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "format_document",
        "file": "src/lib.rs",
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert_eq!(
        std::fs::read_to_string(file_path).unwrap(),
        "pub fn hello_from_lsp() {}\n"
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_range_uses_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub  fn hello_from_lsp() {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "format_range",
        "file": "src/lib.rs",
        "line": 1,
        "column": 4,
        "end_line": 1,
        "end_column": 6
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/rangeFormatting")
    );
    assert_eq!(parsed["result"][0]["newText"].as_str(), Some(" "));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_range_applies_text_edits_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    std::fs::write(&file_path, "pub  fn hello_from_lsp() {}\n").unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "format_range",
        "file": "src/lib.rs",
        "line": 1,
        "column": 4,
        "end_line": 1,
        "end_column": 6,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert_eq!(
        std::fs::read_to_string(file_path).unwrap(),
        "pub fn hello_from_lsp() {}\n"
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_on_type_uses_real_lsp_when_available() {
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
        "operation": "format_on_type",
        "file": "src/lib.rs",
        "line": 1,
        "column": 25,
        "trigger_character": ";"
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/onTypeFormatting")
    );
    assert_eq!(parsed["result"][0]["newText"].as_str(), Some("{\n}"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_format_on_type_applies_text_edits_when_dry_run_false() {
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
        "operation": "format_on_type",
        "file": "src/lib.rs",
        "line": 1,
        "column": 25,
        "trigger_character": ";",
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert_eq!(
        std::fs::read_to_string(file_path).unwrap(),
        "pub fn hello_from_lsp() {\n}\n"
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

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_actions_execute_selected_command_when_dry_run_false() {
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
        "column": 8,
        "action_index": 2,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("workspace/executeCommand"));
    assert_eq!(parsed["executed"].as_bool(), Some(true));
    assert_eq!(
        parsed["result"]["executedCommand"].as_str(),
        Some("fake.applyCommandFix")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_actions_resolve_selected_action_when_dry_run_false() {
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
        "action_index": 3,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    assert!(
        std::fs::read_to_string(file_path)
            .unwrap()
            .contains("hello_from_resolved_fix")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_code_actions_apply_selected_snippet_workspace_edit_when_dry_run_false() {
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
        "action_index": 4,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    let updated = std::fs::read_to_string(file_path).unwrap();
    assert!(updated.contains("hello_snippet"));
    assert!(!updated.contains("$0"), "{updated}");
    assert!(!updated.contains("${"), "{updated}");
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_completions_use_real_lsp_when_available() {
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
        "operation": "completions",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("textDocument/completion"));
    assert_eq!(
        parsed["result"]["items"][0]["label"].as_str(),
        Some("hello_completion")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_completions_resolve_selected_item_when_item_index_provided() {
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
        "operation": "completions",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8,
        "item_index": 1
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(parsed["method"].as_str(), Some("completionItem/resolve"));
    assert_eq!(parsed["selected_index"].as_u64(), Some(1));
    assert_eq!(
        parsed["result"]["documentation"].as_str(),
        Some("resolved completion docs")
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_completions_apply_selected_item_when_dry_run_false() {
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
        "operation": "completions",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8,
        "item_index": 1,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["applied"].as_bool(), Some(true));
    assert_eq!(parsed["files_changed"].as_u64(), Some(1));
    let updated = std::fs::read_to_string(file_path).unwrap();
    assert!(updated.contains("// resolved completion"));
    assert!(updated.contains("resolved_completion(value)"));
    assert!(!updated.contains("${1:value}"));
    assert!(!updated.contains("$0"));
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn lsp_signature_help_uses_real_lsp_when_available() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn hello_from_lsp(name: &str) {}\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "signature_help",
        "file": "src/lib.rs",
        "line": 1,
        "column": 8
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["backend"].as_str(), Some("lsp"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/signatureHelp")
    );
    assert_eq!(
        parsed["result"]["signatures"][0]["label"].as_str(),
        Some("hello_from_lsp(name: &str)")
    );
}

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_signature_help_returns_label_offsets_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn add(left: i32, right: i32) -> i32 { left + right }\n\nfn demo() {\n    let _ = add(1, 2);\n}\n",
    )
    .unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut result = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "signature_help",
            "file": "src/lib.rs",
            "line": 4,
            "column": 18
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        if parsed["result"]["signatures"][0]["parameters"][0]["label"]
            .as_array()
            .is_some_and(|label| label.len() == 2)
        {
            result = Some((candidate, parsed));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let (result, parsed) = result
        .unwrap_or_else(|| panic!("real rust-analyzer never returned signature label offsets"));
    let label = parsed["result"]["signatures"][0]["parameters"][0]["label"]
        .as_array()
        .unwrap_or_else(|| panic!("expected signature parameter label offsets: {result}"));
    assert_eq!(label[0].as_u64(), Some(7));
    assert_eq!(label[1].as_u64(), Some(16));
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
fn lsp_code_lenses_execute_native_rust_analyzer_code_lens_when_dry_run_false() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() { println!(\"native-code-lens-output\"); }\n",
    )
    .unwrap();
    let script = fake_lsp_server_script(dir.path());
    let _cmd_guard = EnvGuard::set("ASTRA_RUST_ANALYZER_CMD", script.to_str().unwrap());
    let _native_lens_guard = EnvGuard::set("FAKE_LSP_RUST_ANALYZER_CODE_LENS", "1");
    let exe = ToolExecutor::new(dir.path());

    let result = exe.lsp(&json!({
        "operation": "code_lenses",
        "file": "src/main.rs",
        "item_index": 0,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["method"].as_str(), Some("textDocument/codeLens"));
    assert_eq!(parsed["source"].as_str(), Some("rust-analyzer-runnables"));
    assert_eq!(parsed["executed"].as_bool(), Some(true));
    assert!(
        parsed["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("native-code-lens-output"))
    );
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

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_completions_apply_selected_item_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    let original = "pub fn demo() {\n    let s = String::new();\n    s.\n}\n";
    std::fs::write(&file_path, original).unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut preview = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "completions",
            "file": "src/lib.rs",
            "line": 3,
            "column": 7
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        let has_items = parsed["result"]
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| parsed["result"].as_array())
            .is_some_and(|items| !items.is_empty());
        if has_items {
            preview = Some((candidate, parsed));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let (preview, parsed) =
        preview.unwrap_or_else(|| panic!("real rust-analyzer never returned completions"));
    let items = parsed["result"]
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| parsed["result"].as_array())
        .unwrap_or_else(|| panic!("unexpected completions preview: {preview}"));
    let idx = items
        .iter()
        .position(|item| item.get("textEdit").is_some())
        .expect("real rust-analyzer should offer at least one applyable completion item");

    let applied = exe.lsp(&json!({
        "operation": "completions",
        "file": "src/lib.rs",
        "line": 3,
        "column": 7,
        "item_index": idx,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(
        parsed["applied"].as_bool(),
        Some(true),
        "unexpected apply result: {applied}"
    );

    let updated = std::fs::read_to_string(file_path).unwrap();
    assert!(
        updated != original,
        "expected completion apply to change the file, got: {updated}"
    );
    assert!(
        !updated.contains("$0"),
        "snippet tabstops should be stripped: {updated}"
    );
    assert!(
        !updated.contains("${"),
        "snippet placeholders should be stripped: {updated}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_hover_returns_markdown_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn demo() {\n    let s = String::new();\n    let _ = s.len();\n}\n",
    )
    .unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut hover = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "hover",
            "file": "src/lib.rs",
            "line": 2,
            "column": 13
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        if parsed["result"]["contents"]["kind"].as_str().is_some() {
            hover = Some((candidate, parsed));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let (hover, parsed) =
        hover.unwrap_or_else(|| panic!("real rust-analyzer never returned hover"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/hover"),
        "{hover}"
    );
    assert_eq!(
        parsed["result"]["contents"]["kind"].as_str(),
        Some("markdown"),
        "{hover}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_hover_actions_return_runnable_commands_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "fn helper() {}\n\n#[test]\nfn smoke() { helper(); }\n",
    )
    .unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut hover = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "hover",
            "file": "src/lib.rs",
            "line": 4,
            "column": 5
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        if parsed["result"]["actions"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            hover = Some((candidate, parsed));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let (hover, parsed) =
        hover.unwrap_or_else(|| panic!("real rust-analyzer never returned hover actions"));
    let actions = parsed["result"]["actions"]
        .as_array()
        .unwrap_or_else(|| panic!("unexpected hover action payload: {hover}"));
    let commands = actions[0]["commands"]
        .as_array()
        .unwrap_or_else(|| panic!("expected hover action commands: {hover}"));
    assert!(
        commands
            .iter()
            .any(|command| { command["command"].as_str() == Some("rust-analyzer.runSingle") }),
        "{hover}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_code_actions_apply_selected_item_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let file_path = dir.path().join("src/lib.rs");
    let original = "pub fn demo() {\n    let _ = HashMap::<i32, i32>::new();\n}\n";
    std::fs::write(&file_path, original).unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut preview = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "code_actions",
            "file": "src/lib.rs",
            "line": 2,
            "column": 14
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        if let Some(actions) = parsed["result"].as_array()
            && !actions.is_empty()
        {
            preview = Some((candidate, parsed));
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let (preview, parsed) =
        preview.unwrap_or_else(|| panic!("real rust-analyzer never returned code actions"));
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/codeAction"),
        "{preview}"
    );
    let actions = parsed["result"]
        .as_array()
        .unwrap_or_else(|| panic!("unexpected code action preview: {preview}"));
    let idx = actions
        .iter()
        .position(|action| {
            action
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|title| title.contains("Import `std::collections::HashMap`"))
        })
        .unwrap_or_else(|| panic!("expected HashMap import quick fix, got: {preview}"));

    let applied = exe.lsp(&json!({
        "operation": "code_actions",
        "file": "src/lib.rs",
        "line": 2,
        "column": 14,
        "action_index": idx,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&applied).unwrap();
    assert_eq!(
        parsed["applied"].as_bool(),
        Some(true),
        "unexpected apply result: {applied}"
    );

    let updated = std::fs::read_to_string(file_path).unwrap();
    assert!(
        updated.contains("use std::collections::HashMap;"),
        "expected import quick fix to update file, got: {updated}"
    );
    assert_ne!(
        updated, original,
        "expected code action apply to modify the file"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "manual validation with real rust-analyzer"]
#[serial_test::serial]
fn lsp_code_lenses_execute_selected_item_with_real_rust_analyzer() {
    if !real_rust_analyzer_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _cmd_guard = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn helper() { println!(\"real-runnable-output\"); }\n\n#[test]\nfn smoke() { helper(); }\n\nfn main() { helper(); }\n",
    )
    .unwrap();
    let exe = ToolExecutor::new(dir.path());

    let mut lenses = None;
    let mut preview = None;
    for _ in 0..12 {
        let candidate = exe.lsp(&json!({
            "operation": "code_lenses",
            "file": "src/main.rs"
        }));
        let parsed: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        if let Some(items) = parsed["result"].as_array()
            && items.iter().any(|lens| {
                lens.get("command")
                    .and_then(|command| command.get("title"))
                    .and_then(Value::as_str)
                    .is_some_and(|title| title.contains("Run"))
            })
        {
            preview = Some(candidate);
            lenses = Some(items.clone());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let Some(lenses) = lenses else {
        return;
    };
    let preview = preview.unwrap();
    let idx = lenses
        .iter()
        .position(|lens| {
            lens.get("command")
                .and_then(|command| command.get("title"))
                .and_then(Value::as_str)
                .is_some_and(|title| title.contains("Run"))
        })
        .unwrap_or(0);

    let executed = exe.lsp(&json!({
        "operation": "code_lenses",
        "file": "src/main.rs",
        "item_index": idx,
        "dry_run": false
    }));
    let parsed: serde_json::Value = serde_json::from_str(&executed).unwrap();
    assert_eq!(
        parsed["method"].as_str(),
        Some("textDocument/codeLens"),
        "unexpected code_lens preview: {preview}; execute result: {executed}"
    );
    assert_eq!(parsed["executed"].as_bool(), Some(true), "{executed}");
    assert!(
        parsed["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("real-runnable-output")),
        "{executed}"
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
