//! Optional **rust-analyzer** over stdio (LSP): sync edited `.rs` files, collect
//! `textDocument/publishDiagnostics`, inject on the next `tool_results` turn.
//!
//! claudecode loads many language servers from **plugins** (`getAllLspServers` →
//! `getPluginLspServers`); each plugin contributes `command` / args / extension map.
//! Here we only wire **rust-analyzer** for Rust workspaces, opt-in via env.
//!
//! - Enable: `MO_AGENT_LSP_RUST=1|true|on|yes`
//! - Binary: `MO_AGENT_RUST_ANALYZER_CMD` (default `rust-analyzer`)

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::sleep;
use url::Url;

const MAX_LSP_DIAG_BATCHES: usize = 32;
const MAX_LSP_LINES_PER_MESSAGE: usize = 120;
const POST_SYNC_DRAIN_MS: u64 = 80;

fn lsp_rust_enabled() -> bool {
    match std::env::var("MO_AGENT_LSP_RUST") {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            v == "1" || v == "true" || v == "on" || v == "yes"
        }
        Err(_) => false,
    }
}

fn rust_analyzer_cmd() -> String {
    std::env::var("MO_AGENT_RUST_ANALYZER_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "rust-analyzer".to_string())
}

#[must_use]
pub(crate) fn should_use_rust_analyzer(project_root: &Path, edited: &Path) -> bool {
    lsp_rust_enabled()
        && edited.extension().and_then(|e| e.to_str()) == Some("rs")
        && project_root.join("Cargo.toml").is_file()
}

fn write_frame(mut w: impl Write, v: &Value) -> io::Result<()> {
    let body =
        serde_json::to_string(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body.as_bytes())?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: BufRead>(r: &mut R) -> io::Result<Value> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "LSP stream closed before headers",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            len = Some(
                rest
                    .trim()
                    .parse()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length"))?,
            );
        }
    }
    let n = len.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn path_to_uri(path: &Path) -> Option<String> {
    let abs = std::fs::canonicalize(path).ok()?;
    Url::from_file_path(&abs).ok().map(|u| u.as_str().to_string())
}

fn workspace_uri(root: &Path) -> String {
    let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Url::from_directory_path(&abs)
        .ok()
        .map(|u| u.as_str().to_string())
        .unwrap_or_else(|| format!("file://{}", abs.display()))
}

type StdinShared = Arc<Mutex<BufWriter<std::process::ChildStdin>>>;

/// Shared session (stdio LSP); created lazily from [`super::ToolExecutor`].
pub(crate) struct RustAnalyzerSession {
    _root: PathBuf,
    _child: Mutex<Option<Child>>,
    stdin: StdinShared,
    versions: Mutex<HashMap<String, i32>>,
    pending_diags: Arc<Mutex<Vec<Value>>>,
}

impl RustAnalyzerSession {
    /// Spawn `rust-analyzer`, run `initialize` / `initialized`, then start a reader thread.
    pub fn try_spawn(root: PathBuf) -> io::Result<Option<Arc<Self>>> {
        let cmd = rust_analyzer_cmd();
        let mut child = match Command::new(&cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut r = BufReader::new(stderr);
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
            });
        }

        let stdin_raw = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = Arc::new(Mutex::new(BufWriter::new(stdin_raw)));
        let mut reader = BufReader::new(stdout);

        let root_uri = workspace_uri(&root);
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into());

        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true
                    },
                    "textDocument": {
                        "synchronization": {
                            "dynamicRegistration": false,
                            "willSave": false,
                            "willSaveWaitUntil": false,
                            "didSave": true
                        },
                        "publishDiagnostics": {}
                    }
                },
                "workspaceFolders": [{ "uri": root_uri, "name": name }]
            }
        });
        {
            let mut w = stdin
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdin mutex poisoned"))?;
            write_frame(&mut *w, &init)?;
        }

        loop {
            let msg = read_frame(&mut reader)?;
            let id_ok = msg.get("id").and_then(|v| v.as_u64()) == Some(1)
                || msg.get("id").and_then(|v| v.as_i64()) == Some(1);
            if id_ok && (msg.get("result").is_some() || msg.get("error").is_some()) {
                break;
            }
        }

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        {
            let mut w = stdin
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdin mutex poisoned"))?;
            write_frame(&mut *w, &initialized)?;
        }

        let pending_diags = Arc::new(Mutex::new(Vec::new()));
        let pending_clone = Arc::clone(&pending_diags);
        let stdin_reader = Arc::clone(&stdin);
        thread::spawn(move || {
            reader_loop(reader, stdin_reader, pending_clone);
        });

        Ok(Some(Arc::new(Self {
            _root: root,
            _child: Mutex::new(Some(child)),
            stdin,
            versions: Mutex::new(HashMap::new()),
            pending_diags,
        })))
    }

    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut w = self
            .stdin
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdin mutex poisoned"))?;
        write_frame(&mut *w, &msg)
    }

    /// Re-read disk and push LSP sync + `didSave` (triggers analysis / diagnostics).
    pub fn sync_document_from_disk(&self, path: &Path) -> io::Result<()> {
        let Some(uri) = path_to_uri(path) else {
            return Ok(());
        };
        let text = std::fs::read_to_string(path)?;
        let mut versions = self
            .versions
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "versions mutex poisoned"))?;
        let is_open = versions.contains_key(&uri);
        if !is_open {
            versions.insert(uri.clone(), 1);
            drop(versions);
            self.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri.clone(),
                        "languageId": "rust",
                        "version": 1,
                        "text": text
                    }
                }),
            )?;
        } else {
            let v = versions.get_mut(&uri).expect("uri present");
            *v += 1;
            let version = *v;
            drop(versions);
            self.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri.clone(), "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            )?;
        }
        self.send_notification(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        Ok(())
    }

    fn send_response(stdin: &StdinShared, id: &Value, result: Value) -> io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let mut w = stdin
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdin mutex poisoned"))?;
        write_frame(&mut *w, &msg)
    }

    pub fn take_formatted_diagnostic_messages(&self) -> Vec<Value> {
        let mut batches = match self.pending_diags.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => return Vec::new(),
        };
        if batches.is_empty() {
            return Vec::new();
        }
        if batches.len() > MAX_LSP_DIAG_BATCHES {
            batches.truncate(MAX_LSP_DIAG_BATCHES);
        }
        let mut lines: Vec<String> = Vec::new();
        for params in batches {
            if let Some(uri) = params.get("uri").and_then(|u| u.as_str()) {
                let path_hint = uri.strip_prefix("file://").unwrap_or(uri);
                lines.push(format!("── {path_hint} ──"));
            }
            if let Some(diags) = params.get("diagnostics").and_then(|d| d.as_array()) {
                for d in diags.iter().take(40) {
                    let sev = d
                        .get("severity")
                        .and_then(|x| x.as_u64())
                        .map(|n| match n {
                            1 => "error",
                            2 => "warning",
                            3 => "info",
                            4 => "hint",
                            _ => "?",
                        })
                        .unwrap_or("diag");
                    let msg = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    let range = d.get("range");
                    let loc = range
                        .and_then(|r| r.get("start"))
                        .map(|s| {
                            let l = s.get("line").and_then(|x| x.as_u64()).unwrap_or(0);
                            let c = s.get("character").and_then(|x| x.as_u64()).unwrap_or(0);
                            format!("{}:{}", l + 1, c + 1)
                        })
                        .unwrap_or_else(|| "?".into());
                    lines.push(format!("  [{sev}] {loc}  {msg}"));
                }
            }
            lines.push(String::new());
        }
        while lines.len() > MAX_LSP_LINES_PER_MESSAGE {
            lines.pop();
        }
        let body = lines.join("\n").trim().to_string();
        if body.is_empty() {
            return Vec::new();
        }
        vec![json!({
            "role": "user",
            "content": format!("<new-diagnostics>\nrust-analyzer (LSP) diagnostics:\n\n{body}\n</new-diagnostics>"),
            "attachment_metadata": {
                "kind": "passive_workspace_diagnostics",
                "source": "rust_analyzer_lsp",
            }
        })]
    }
}

impl Drop for RustAnalyzerSession {
    fn drop(&mut self) {
        if let Ok(mut c) = self._child.lock() {
            if let Some(mut ch) = c.take() {
                let _ = ch.kill();
                let _ = ch.wait();
            }
        }
    }
}

fn reader_loop(mut reader: BufReader<std::process::ChildStdout>, stdin: StdinShared, pending: Arc<Mutex<Vec<Value>>>) {
    loop {
        let msg = match read_frame(&mut reader) {
            Ok(m) => m,
            Err(_) => break,
        };
        if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(p) = msg.get("params").cloned() {
                    if let Ok(mut g) = pending.lock() {
                        g.push(p);
                        if g.len() > MAX_LSP_DIAG_BATCHES * 2 {
                            let drain = g.len() - MAX_LSP_DIAG_BATCHES;
                            g.drain(0..drain);
                        }
                    }
                }
                continue;
            }
            if let Some(id) = msg.get("id") {
                let result = match method {
                    "workspace/configuration" => {
                        let n = msg
                            .pointer("/params/items")
                            .and_then(|x| x.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0);
                        Value::Array(vec![Value::Null; n])
                    }
                    "client/registerCapability" | "client/unregisterCapability" => Value::Null,
                    _ => Value::Null,
                };
                let _ = RustAnalyzerSession::send_response(&stdin, id, result);
            }
        }
    }
}

/// Ensure session exists, then sync one file from disk.
pub(crate) fn sync_after_write(session_slot: &Mutex<Option<Arc<RustAnalyzerSession>>>, root: &Path, path: &Path) {
    if !should_use_rust_analyzer(root, path) {
        return;
    }
    let sess = {
        let mut slot = match session_slot.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if slot.is_none() {
            match RustAnalyzerSession::try_spawn(root.to_path_buf()) {
                Ok(Some(s)) => *slot = Some(Arc::clone(&s)),
                Ok(None) | Err(_) => return,
            }
        }
        slot.as_ref().map(Arc::clone)
    };
    let Some(sess) = sess else {
        return;
    };
    let _ = sess.sync_document_from_disk(path);
}

pub(crate) async fn take_rust_analyzer_messages(
    session_slot: &Mutex<Option<Arc<RustAnalyzerSession>>>,
    tool_results_nonempty: bool,
) -> Vec<Value> {
    if !lsp_rust_enabled() || !tool_results_nonempty {
        return Vec::new();
    }
    let sess = match session_slot.lock() {
        Ok(g) => g.as_ref().map(Arc::clone),
        Err(_) => return Vec::new(),
    };
    let Some(sess) = sess else {
        return Vec::new();
    };
    sleep(Duration::from_millis(POST_SYNC_DRAIN_MS)).await;
    sess.take_formatted_diagnostic_messages()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_use_requires_env_rs_and_cargo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert!(!should_use_rust_analyzer(root, Path::new("src/a.rs")));
        struct SetEnv;
        impl Drop for SetEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("MO_AGENT_LSP_RUST");
                }
            }
        }
        unsafe {
            std::env::set_var("MO_AGENT_LSP_RUST", "1");
        }
        let _g = SetEnv;
        assert!(should_use_rust_analyzer(root, Path::new("src/a.rs")));
        assert!(!should_use_rust_analyzer(root, Path::new("src/a.ts")));
    }

    #[tokio::test]
    async fn no_session_means_no_messages() {
        let slot = Mutex::new(None);
        let v = take_rust_analyzer_messages(&slot, true).await;
        assert!(v.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_sync_and_drain_smoke() {
        let Ok(status) = Command::new(rust_analyzer_cmd())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return;
        };
        if !status.success() {
            return;
        }
        struct SetEnv;
        impl Drop for SetEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("MO_AGENT_LSP_RUST");
                }
            }
        }
        unsafe {
            std::env::set_var("MO_AGENT_LSP_RUST", "1");
        }
        let _g = SetEnv;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"ra_smoke\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let sess = match RustAnalyzerSession::try_spawn(root.clone()) {
            Ok(Some(s)) => s,
            Ok(None) | Err(_) => return,
        };
        sess.sync_document_from_disk(&root.join("src/lib.rs")).expect("sync");
        sleep(Duration::from_millis(500)).await;
        let msgs = sess.take_formatted_diagnostic_messages();
        // May be empty if analysis clean; if non-empty, must be LSP-shaped.
        for m in msgs {
            assert!(m["content"].as_str().unwrap().contains("rust-analyzer"));
        }
    }
}
