//! Generic stdio LSP client: Content-Length framing, `initialize` / `initialized`,
//! background reader for `textDocument/publishDiagnostics` and minimal server requests.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use url::Url;

pub(crate) const MAX_LSP_DIAG_BATCHES: usize = 32;
pub(crate) const MAX_LSP_DIAG_FILES_PER_MESSAGE: usize = 8;
pub(crate) const MAX_LSP_DIAGS_PER_FILE: usize = 12;
pub(crate) const MAX_LSP_LINES_PER_MESSAGE: usize = 120;

#[derive(Clone, Copy, Debug)]
pub(crate) enum LanguageIdPolicy {
    Fixed(&'static str),
    /// `.ts` → `typescript`, `.tsx` → `typescriptreact`
    TypeScript,
}

impl LanguageIdPolicy {
    fn language_id(self, path: &Path) -> Option<String> {
        match self {
            Self::Fixed(s) => Some(s.to_string()),
            Self::TypeScript => match path.extension()?.to_str()? {
                "ts" => Some("typescript".into()),
                "tsx" => Some("typescriptreact".into()),
                _ => None,
            },
        }
    }
}

/// How to launch one language server (one OS process per [`LspStdioSession`]).
pub(crate) struct LspSpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub diagnostic_title: &'static str,
    pub attachment_source: &'static str,
    pub language_policy: LanguageIdPolicy,
    pub initialization_options: Option<Value>,
    pub configuration_section: Option<&'static str>,
    pub configuration_value: Option<Value>,
    pub did_change_configuration: Option<Value>,
    pub experimental_capabilities: Option<Value>,
}

type StdinShared = Arc<Mutex<BufWriter<std::process::ChildStdin>>>;

#[derive(Clone, Debug)]
struct SyncedDocumentState {
    version: i32,
    last_mtime_ms: u128,
    last_text_hash: u64,
}

pub(crate) struct LspStdioSession {
    diagnostic_title: &'static str,
    attachment_source: &'static str,
    language_policy: LanguageIdPolicy,
    _root: PathBuf,
    _child: Mutex<Option<Child>>,
    stdin: StdinShared,
    documents: Mutex<HashMap<String, SyncedDocumentState>>,
    pending_diags: Arc<Mutex<Vec<Value>>>,
    latest_diags: Arc<Mutex<HashMap<String, Value>>>,
    emitted_diag_hashes: Mutex<HashMap<String, u64>>,
    next_request_id: AtomicU64,
    pending_requests: Arc<Mutex<HashMap<u64, Sender<io::Result<Value>>>>>,
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
            len =
                Some(rest.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length")
                })?);
        }
    }
    let n =
        len.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub(crate) fn path_to_uri(path: &Path) -> Option<String> {
    let abs = std::fs::canonicalize(path).ok()?;
    Url::from_file_path(&abs)
        .ok()
        .map(|u| u.as_str().to_string())
}

fn workspace_uri(root: &Path) -> String {
    let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    Url::from_directory_path(&abs)
        .ok()
        .map(|u| u.as_str().to_string())
        .unwrap_or_else(|| format!("file://{}", abs.display()))
}

fn file_mtime_ms(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn text_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn diagnostic_payload_hash(value: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn diagnostic_severity_label(severity: Option<u64>) -> &'static str {
    match severity {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "diag",
    }
}

fn collect_diagnostic_entries(diags: &[Value]) -> Vec<(String, String, String)> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for d in diags {
        let severity = diagnostic_severity_label(d.get("severity").and_then(Value::as_u64));
        let message = d
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let location = d
            .get("range")
            .and_then(|r| r.get("start"))
            .map(|s| {
                let line = s.get("line").and_then(Value::as_u64).unwrap_or(0);
                let character = s.get("character").and_then(Value::as_u64).unwrap_or(0);
                format!("{}:{}", line + 1, character + 1)
            })
            .unwrap_or_else(|| "?".to_string());
        let fingerprint = format!("{severity}|{location}|{message}");
        if seen.insert(fingerprint) {
            entries.push((severity.to_string(), location, message));
        }
    }
    entries
}

fn summarize_diagnostic_entries(entries: &[(String, String, String)]) -> String {
    if entries.is_empty() {
        return "cleared".to_string();
    }

    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut infos = 0usize;
    let mut hints = 0usize;
    let mut others = 0usize;
    for (severity, _, _) in entries {
        match severity.as_str() {
            "error" => errors += 1,
            "warning" => warnings += 1,
            "info" => infos += 1,
            "hint" => hints += 1,
            _ => others += 1,
        }
    }

    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(pluralize(errors, "error", "errors"));
    }
    if warnings > 0 {
        parts.push(pluralize(warnings, "warning", "warnings"));
    }
    if infos > 0 {
        parts.push(pluralize(infos, "info", "infos"));
    }
    if hints > 0 {
        parts.push(pluralize(hints, "hint", "hints"));
    }
    if others > 0 {
        parts.push(pluralize(others, "diagnostic", "diagnostics"));
    }

    if parts.is_empty() {
        pluralize(entries.len(), "diagnostic", "diagnostics")
    } else {
        format!(
            "{}: {}",
            pluralize(entries.len(), "diagnostic", "diagnostics"),
            parts.join(", ")
        )
    }
}

fn coalesce_diagnostic_batches(batches: Vec<Value>) -> Vec<Value> {
    let mut by_uri: HashMap<String, (usize, Value)> = HashMap::new();
    let mut without_uri = Vec::new();
    for (idx, batch) in batches.into_iter().enumerate() {
        if let Some(uri) = batch.get("uri").and_then(Value::as_str) {
            by_uri.insert(uri.to_string(), (idx, batch));
        } else {
            without_uri.push((idx, batch));
        }
    }
    let mut merged: Vec<(usize, Value)> = by_uri.into_values().collect();
    merged.extend(without_uri);
    merged.sort_by_key(|(idx, _)| *idx);
    merged.into_iter().map(|(_, batch)| batch).collect()
}

fn render_diagnostic_messages(
    title: &str,
    source: &str,
    batches: Vec<Value>,
    emitted_hashes: &mut HashMap<String, u64>,
) -> Vec<Value> {
    let mut lines: Vec<String> = Vec::new();
    let mut rendered_files = 0usize;
    let mut omitted_files = 0usize;

    for params in coalesce_diagnostic_batches(batches) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entries = collect_diagnostic_entries(&diagnostics);
        let signature = diagnostic_payload_hash(&json!(
            entries
                .iter()
                .map(|(severity, location, message)| {
                    json!({
                        "severity": severity,
                        "location": location,
                        "message": message,
                    })
                })
                .collect::<Vec<_>>()
        ));
        let previous_signature = emitted_hashes.get(uri).copied();
        if previous_signature == Some(signature) {
            continue;
        }
        emitted_hashes.insert(uri.to_string(), signature);

        let path_hint = uri.strip_prefix("file://").unwrap_or(uri);
        if diagnostics.is_empty() {
            if previous_signature.is_none() {
                continue;
            }
            lines.push(format!("── {path_hint} (cleared) ──"));
            lines.push("  diagnostics cleared".to_string());
            lines.push(String::new());
            continue;
        }

        if rendered_files >= MAX_LSP_DIAG_FILES_PER_MESSAGE {
            omitted_files += 1;
            continue;
        }
        rendered_files += 1;
        lines.push(format!(
            "── {path_hint} ({}) ──",
            summarize_diagnostic_entries(&entries)
        ));

        let mut omitted_entries = 0usize;
        for (idx, (severity, location, message)) in entries.iter().enumerate() {
            if idx >= MAX_LSP_DIAGS_PER_FILE {
                omitted_entries += 1;
                continue;
            }
            lines.push(format!("  [{severity}] {location}  {message}"));
        }
        if omitted_entries > 0 {
            lines.push(format!("  … {} more diagnostics omitted", omitted_entries));
        }
        lines.push(String::new());
    }

    if omitted_files > 0 {
        lines.push(format!(
            "[{} additional files with changed diagnostics omitted]",
            omitted_files
        ));
        lines.push(String::new());
    }

    if lines.len() > MAX_LSP_LINES_PER_MESSAGE {
        let keep = MAX_LSP_LINES_PER_MESSAGE.saturating_sub(1);
        lines.truncate(keep);
        lines.push(format!(
            "[truncated LSP diagnostics at {MAX_LSP_LINES_PER_MESSAGE} lines]"
        ));
    }

    let body = lines.join("\n").trim().to_string();
    if body.is_empty() {
        return Vec::new();
    }

    vec![json!({
        "role": "user",
        "content": format!("<new-diagnostics>\n{title} (LSP) diagnostics:\n\n{body}\n</new-diagnostics>"),
        "attachment_metadata": {
            "kind": "passive_workspace_diagnostics",
            "source": source,
        }
    })]
}

fn sync_existing_document(doc: &mut SyncedDocumentState, mtime_ms: u128, hash: u64) -> Option<i32> {
    if doc.last_text_hash == hash {
        doc.last_mtime_ms = mtime_ms;
        return None;
    }
    doc.version += 1;
    doc.last_mtime_ms = mtime_ms;
    doc.last_text_hash = hash;
    Some(doc.version)
}

fn send_lsp_response(stdin: &StdinShared, id: &Value, result: Value) -> io::Result<()> {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let mut w = stdin
        .lock()
        .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
    write_frame(&mut *w, &msg)
}

fn reader_loop(
    mut reader: BufReader<std::process::ChildStdout>,
    stdin: StdinShared,
    pending: Arc<Mutex<Vec<Value>>>,
    latest_diags: Arc<Mutex<HashMap<String, Value>>>,
    pending_requests: Arc<Mutex<HashMap<u64, Sender<io::Result<Value>>>>>,
    configuration_section: Option<String>,
    configuration_value: Option<Value>,
) {
    while let Ok(msg) = read_frame(&mut reader) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64)
            && (msg.get("result").is_some() || msg.get("error").is_some())
        {
            if let Ok(mut reqs) = pending_requests.lock()
                && let Some(tx) = reqs.remove(&id)
            {
                let send_result = if let Some(err) = msg.get("error") {
                    tx.send(Err(io::Error::other(format!("LSP request failed: {err}"))))
                } else {
                    tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)))
                };
                let _ = send_result;
            }
            continue;
        }
        if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(p) = msg.get("params").cloned() {
                    if let Some(uri) = p.get("uri").and_then(Value::as_str)
                        && let Ok(mut latest) = latest_diags.lock()
                    {
                        latest.insert(uri.to_string(), p.clone());
                    }
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
                        let items = msg
                            .pointer("/params/items")
                            .and_then(|x| x.as_array())
                            .cloned()
                            .unwrap_or_default();
                        Value::Array(
                            items
                                .into_iter()
                                .map(|item| {
                                    if let Some(section) =
                                        item.get("section").and_then(Value::as_str)
                                        && configuration_section.as_deref() == Some(section)
                                    {
                                        configuration_value.clone().unwrap_or_else(|| json!({}))
                                    } else {
                                        Value::Null
                                    }
                                })
                                .collect(),
                        )
                    }
                    "client/registerCapability" | "client/unregisterCapability" => Value::Null,
                    "window/workDoneProgress/create" => Value::Null,
                    _ => Value::Null,
                };
                let _ = send_lsp_response(&stdin, id, result);
            }
        }
    }
}

fn startup_stderr(stderr_rx: &Receiver<String>) -> Option<String> {
    stderr_rx
        .recv_timeout(Duration::from_millis(200))
        .ok()
        .map(|stderr| stderr.trim().to_string())
        .filter(|stderr| !stderr.is_empty())
}

impl LspStdioSession {
    pub fn try_spawn(root: PathBuf, spec: LspSpawnSpec) -> io::Result<Option<Arc<Self>>> {
        let LspSpawnSpec {
            command,
            args,
            diagnostic_title,
            attachment_source,
            language_policy,
            initialization_options,
            configuration_section,
            configuration_value,
            did_change_configuration,
            experimental_capabilities,
        } = spec;

        let mut child = match Command::new(&command)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        let mut stderr_rx = None;
        if let Some(stderr) = child.stderr.take() {
            let (tx, rx) = mpsc::channel();
            stderr_rx = Some(rx);
            thread::spawn(move || {
                let mut r = BufReader::new(stderr);
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
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

        let mut capabilities = json!({
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
                "publishDiagnostics": {},
                "hover": {
                    "contentFormat": ["markdown", "plaintext"]
                },
                "completion": {
                    "completionItem": {
                        "snippetSupport": true,
                        "insertReplaceSupport": true,
                        "labelDetailsSupport": true,
                        "resolveSupport": {
                            "properties": [
                                "documentation",
                                "detail",
                                "additionalTextEdits"
                            ]
                        }
                    }
                },
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": {
                            "valueSet": [
                                "quickfix",
                                "refactor",
                                "refactor.extract",
                                "refactor.inline",
                                "refactor.rewrite",
                                "source"
                            ]
                        }
                    }
                }
            }
        });
        if let Some(experimental) = experimental_capabilities
            && let Some(root) = capabilities.as_object_mut()
        {
            root.insert("experimental".to_string(), experimental);
        }
        let mut init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": capabilities,
                "workspaceFolders": [{ "uri": root_uri, "name": name }]
            }
        });
        if let Some(initialization_options) = initialization_options
            && let Some(params) = init.get_mut("params").and_then(Value::as_object_mut)
        {
            params.insert("initializationOptions".to_string(), initialization_options);
        }
        {
            let mut w = stdin
                .lock()
                .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
            write_frame(&mut *w, &init)?;
        }

        loop {
            let msg = match read_frame(&mut reader) {
                Ok(msg) => msg,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(stderr) = stderr_rx.as_ref().and_then(startup_stderr) {
                        return Err(io::Error::other(format!("{error}; stderr: {stderr}")));
                    }
                    return Err(error);
                }
            };
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
                .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
            write_frame(&mut *w, &initialized)?;
        }
        if let Some(settings) = did_change_configuration {
            let did_change_configuration = json!({
                "jsonrpc": "2.0",
                "method": "workspace/didChangeConfiguration",
                "params": {
                    "settings": settings,
                }
            });
            let mut w = stdin
                .lock()
                .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
            write_frame(&mut *w, &did_change_configuration)?;
        }

        let pending_diags = Arc::new(Mutex::new(Vec::new()));
        let pending_clone = Arc::clone(&pending_diags);
        let latest_diags = Arc::new(Mutex::new(HashMap::new()));
        let latest_diags_reader = Arc::clone(&latest_diags);
        let stdin_reader = Arc::clone(&stdin);
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let pending_requests_reader = Arc::clone(&pending_requests);
        thread::spawn(move || {
            reader_loop(
                reader,
                stdin_reader,
                pending_clone,
                latest_diags_reader,
                pending_requests_reader,
                configuration_section.map(str::to_string),
                configuration_value,
            );
        });

        Ok(Some(Arc::new(Self {
            diagnostic_title,
            attachment_source,
            language_policy,
            _root: root,
            _child: Mutex::new(Some(child)),
            stdin,
            documents: Mutex::new(HashMap::new()),
            pending_diags,
            latest_diags,
            emitted_diag_hashes: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(2),
            pending_requests,
        })))
    }

    fn send_notification(&self, method: &str, params: Value) -> io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut w = self
            .stdin
            .lock()
            .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
        write_frame(&mut *w, &msg)
    }

    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> io::Result<Value> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self
                .pending_requests
                .lock()
                .map_err(|_| io::Error::other("pending_requests mutex poisoned"))?;
            pending.insert(id, tx);
        }

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let write_result = {
            let mut w = self
                .stdin
                .lock()
                .map_err(|_| io::Error::other("stdin mutex poisoned"))?;
            write_frame(&mut *w, &msg)
        };
        if let Err(err) = write_result {
            if let Ok(mut pending) = self.pending_requests.lock() {
                pending.remove(&id);
            }
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut pending) = self.pending_requests.lock() {
                    pending.remove(&id);
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("LSP request timed out: {method}"),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("LSP request channel closed: {method}"),
            )),
        }
    }

    fn sync_document(&self, path: &Path, text: &str, mtime_ms: u128) -> io::Result<()> {
        let Some(lang) = self.language_policy.language_id(path) else {
            return Ok(());
        };
        let Some(uri) = path_to_uri(path) else {
            return Ok(());
        };
        let hash = text_hash(text);
        enum SyncAction {
            Open {
                version: i32,
                language_id: String,
                next_state: SyncedDocumentState,
            },
            Change {
                version: i32,
                next_state: SyncedDocumentState,
            },
            Noop,
        }
        let action = {
            let mut documents = self
                .documents
                .lock()
                .map_err(|_| io::Error::other("documents mutex poisoned"))?;
            match documents.get(&uri).cloned() {
                Some(doc) => {
                    let mut next_state = doc.clone();
                    match sync_existing_document(&mut next_state, mtime_ms, hash) {
                        Some(version) => SyncAction::Change {
                            version,
                            next_state,
                        },
                        None => {
                            documents.insert(uri.clone(), next_state);
                            SyncAction::Noop
                        }
                    }
                }
                None => {
                    let next_state = SyncedDocumentState {
                        version: 1,
                        last_mtime_ms: mtime_ms,
                        last_text_hash: hash,
                    };
                    SyncAction::Open {
                        version: 1,
                        language_id: lang,
                        next_state,
                    }
                }
            }
        };
        let committed_state = match action {
            SyncAction::Open {
                version,
                language_id,
                next_state,
            } => {
                self.send_notification(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri.clone(),
                            "languageId": language_id,
                            "version": version,
                            "text": text
                        }
                    }),
                )?;
                self.send_notification(
                    "textDocument/didSave",
                    json!({ "textDocument": { "uri": uri.clone() } }),
                )?;
                Some(next_state)
            }
            SyncAction::Change {
                version,
                next_state,
            } => {
                self.send_notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri.clone(), "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                )?;
                self.send_notification(
                    "textDocument/didSave",
                    json!({ "textDocument": { "uri": uri.clone() } }),
                )?;
                Some(next_state)
            }
            SyncAction::Noop => None,
        };
        if let Some(next_state) = committed_state {
            let mut documents = self
                .documents
                .lock()
                .map_err(|_| io::Error::other("documents mutex poisoned"))?;
            documents.insert(uri, next_state);
        }
        Ok(())
    }

    pub fn sync_document_text(&self, path: &Path, text: &str) -> io::Result<()> {
        self.sync_document(path, text, file_mtime_ms(path))
    }

    pub fn sync_document_from_disk(&self, path: &Path) -> io::Result<()> {
        let text = std::fs::read_to_string(path)?;
        self.sync_document(path, &text, file_mtime_ms(path))
    }

    pub fn latest_diagnostics_for_path(&self, path: &Path) -> io::Result<Value> {
        let Some(uri) = path_to_uri(path) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Failed to create file URI for {}", path.display()),
            ));
        };
        let latest = self
            .latest_diags
            .lock()
            .map_err(|_| io::Error::other("latest_diags mutex poisoned"))?;
        Ok(latest
            .get(&uri)
            .cloned()
            .unwrap_or_else(|| json!({ "uri": uri, "diagnostics": [] })))
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
        let mut emitted_hashes = match self.emitted_diag_hashes.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        render_diagnostic_messages(
            self.diagnostic_title,
            self.attachment_source,
            batches,
            &mut emitted_hashes,
        )
    }
}

impl Drop for LspStdioSession {
    fn drop(&mut self) {
        if let Ok(mut c) = self._child.lock()
            && let Some(mut ch) = c.take()
        {
            let _ = ch.kill();
            let _ = ch.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_match(haystack: &str, needle: &str) -> usize {
        haystack.match_indices(needle).count()
    }

    #[test]
    fn lsp_render_diagnostic_messages_coalesces_latest_and_skips_repeats() {
        let mut emitted = HashMap::new();
        let messages = render_diagnostic_messages(
            "rust-analyzer",
            "rust_analyzer_lsp",
            vec![
                json!({
                    "uri": "file:///demo.rs",
                    "diagnostics": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1}
                        },
                        "severity": 2,
                        "message": "old warning"
                    }]
                }),
                json!({
                    "uri": "file:///demo.rs",
                    "diagnostics": [
                        {
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1}
                            },
                            "severity": 1,
                            "message": "new error"
                        },
                        {
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1}
                            },
                            "severity": 1,
                            "message": "new error"
                        }
                    ]
                }),
            ],
            &mut emitted,
        );
        assert_eq!(messages.len(), 1);
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.contains("demo.rs (1 diagnostic: 1 error)"));
        assert!(content.contains("[error] 1:1  new error"));
        assert!(!content.contains("old warning"));
        assert_eq!(count_match(content, "new error"), 1);

        let repeated = render_diagnostic_messages(
            "rust-analyzer",
            "rust_analyzer_lsp",
            vec![json!({
                "uri": "file:///demo.rs",
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "severity": 1,
                    "message": "new error"
                }]
            })],
            &mut emitted,
        );
        assert!(repeated.is_empty());
    }

    #[test]
    fn lsp_render_diagnostic_messages_reports_clears_once() {
        let mut emitted = HashMap::new();
        let first = render_diagnostic_messages(
            "rust-analyzer",
            "rust_analyzer_lsp",
            vec![json!({
                "uri": "file:///demo.rs",
                "diagnostics": [{
                    "range": {
                        "start": {"line": 1, "character": 2},
                        "end": {"line": 1, "character": 3}
                    },
                    "severity": 2,
                    "message": "warn"
                }]
            })],
            &mut emitted,
        );
        assert_eq!(first.len(), 1);

        let cleared = render_diagnostic_messages(
            "rust-analyzer",
            "rust_analyzer_lsp",
            vec![json!({
                "uri": "file:///demo.rs",
                "diagnostics": []
            })],
            &mut emitted,
        );
        assert_eq!(cleared.len(), 1);
        let content = cleared[0]["content"].as_str().unwrap();
        assert!(content.contains("demo.rs (cleared)"));
        assert!(content.contains("diagnostics cleared"));

        let repeated_clear = render_diagnostic_messages(
            "rust-analyzer",
            "rust_analyzer_lsp",
            vec![json!({
                "uri": "file:///demo.rs",
                "diagnostics": []
            })],
            &mut emitted,
        );
        assert!(repeated_clear.is_empty());
    }

    #[test]
    fn lsp_sync_existing_document_resyncs_when_hash_changes_but_mtime_does_not() {
        let mut doc = SyncedDocumentState {
            version: 1,
            last_mtime_ms: 1000,
            last_text_hash: 11,
        };

        let result = sync_existing_document(&mut doc, 1000, 22);

        assert_eq!(result, Some(2));
        assert_eq!(doc.version, 2);
        assert_eq!(doc.last_mtime_ms, 1000);
        assert_eq!(doc.last_text_hash, 22);
    }

    #[test]
    fn lsp_sync_existing_document_skips_resync_when_hash_matches_even_if_mtime_changes() {
        let mut doc = SyncedDocumentState {
            version: 3,
            last_mtime_ms: 1000,
            last_text_hash: 33,
        };

        let result = sync_existing_document(&mut doc, 2000, 33);

        assert_eq!(result, None);
        assert_eq!(doc.version, 3);
        assert_eq!(doc.last_mtime_ms, 2000);
        assert_eq!(doc.last_text_hash, 33);
    }
}
