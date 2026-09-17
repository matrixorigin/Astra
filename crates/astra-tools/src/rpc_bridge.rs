//! Shared primitives and policy-parameterized RPC server for the script
//! bridge used by `run_script`.
//!
//! What lives here:
//! - [`AuthToken`]: constant-time-equality per-invocation random token.
//! - [`kill_process_group`]: Unix SIGKILL to the child's entire process
//!   group, relying on the `setsid` pre_exec hook.
//! - [`RpcPolicy`]: caller-provided allowlist / call-count / response-size
//!   policy for a single script run.
//! - [`handle_rpc_connection`]: the full RPC server loop body, reading one
//!   JSON-line request, enforcing policy, dispatching to a `ToolExecutor`,
//!   and writing back a response. Used by `run_script`.
//! - [`write_response`]: infallible response writer that never hangs the
//!   client script (uses a hardcoded fallback on serialize failure).
//!
//! ### Security posture
//!
//! - Auth token is *mandatory* at the deserialization boundary: a request
//!   missing `auth_token` fails to parse before any dispatch logic runs.
//! - Tool name is validated against `policy.allowed_tools` AFTER auth.
//! - Response payloads are truncated server-side to `policy.max_response_bytes`
//!   to prevent an unbounded tool result from OOM'ing the sandbox child.
//! - Auth failures are logged at `debug` (LLM-controlled content; warn-level
//!   would flood operator logs on a hostile script).

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::ToolExecutor;

tokio::task_local! {
    /// Marks a tool invocation dispatched by an authenticated `run_script`
    /// RPC request.  The marker is scoped to the dispatch future rather than
    /// stored on a shared executor, so concurrent top-level calls cannot
    /// inherit the parent's re-entrant writer privilege accidentally.
    static RUN_SCRIPT_RPC_DISPATCH: ();
}

/// Whether the current tool call is executing inside `run_script`'s RPC
/// bridge.
///
/// Workspace tools use this to reuse the outer script's exclusive writer
/// generation. This is only a coordination fact: a nested Bash
/// must still avoid minting a fingerprint receipt because its parent Python
/// process can access the workspace concurrently.
pub fn is_run_script_rpc_dispatch() -> bool {
    RUN_SCRIPT_RPC_DISPATCH.try_with(|()| ()).is_ok()
}

// ─── AuthToken ────────────────────────────────────────────────────────────

/// Per-invocation RPC authentication token. 128 bits of entropy from a
/// random UUID (hex, 32 chars). Wraps a String so equality is constant-time
/// and so the type system prevents mix-ups with unrelated strings.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl fmt::Debug for AuthToken {
    /// Intentionally do not print the token bytes — debug logs from tool
    /// errors sometimes land in user-visible surfaces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AuthToken(<redacted {} chars>)", self.0.len())
    }
}

impl AuthToken {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn constant_time_eq(&self, other: &AuthToken) -> bool {
        let a = self.0.as_bytes();
        let b = other.0.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    #[cfg(test)]
    pub fn from_str_for_test(s: &str) -> Self {
        Self(s.to_string())
    }
}

// ─── Process group kill ───────────────────────────────────────────────────

/// Kill the entire process group of `child` via SIGKILL.
/// Relies on the caller having set `setsid()` in the child's `pre_exec` so
/// that the child's PID is also its PGID.
#[cfg(unix)]
pub(crate) fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_process_group_id(pid);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_child: &tokio::process::Child) {}

/// Kill a process group when the leader has already been reaped and therefore
/// no longer has an id available through `Child::id`. The caller must only
/// pass a PID it created and placed in its own process group (the run-script
/// and shell launchers use `setsid`/`process_group(0)` for that invariant).
#[cfg(unix)]
pub(crate) fn kill_process_group_id(pid: u32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 1
    {
        // SAFETY: the negative PID targets the invocation-owned process
        // group, never an arbitrary process. SIGKILL is intentional during
        // terminal cleanup so descendants cannot outlive the tool lease.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group_id(_pid: u32) {}

// ─── Protocol types ───────────────────────────────────────────────────────

/// Maximum RPC request line size (OOM prevention on request path).
pub(crate) const MAX_RPC_REQUEST_BYTES: u64 = 1_024 * 1_024;

/// JSON RPC request from the sandboxed script.
/// `auth_token` is required at parse time — missing ⇒ deserialize error.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct RpcRequest {
    pub tool: String,
    pub args: Value,
    pub auth_token: AuthToken,
}

/// JSON RPC response to the sandboxed script.
#[derive(Debug, serde::Serialize)]
pub(crate) struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn success(output: String) -> Self {
        Self {
            output: Some(output),
            error: None,
        }
    }
    pub fn error(msg: String) -> Self {
        Self {
            output: None,
            error: Some(msg),
        }
    }
}

/// Fallback response bytes written if serde_json::to_vec ever fails on a
/// `RpcResponse` (should be impossible — all fields are JSON-valid strings
/// — but writing *something* is always better than hanging the script).
const FALLBACK_RESPONSE_BYTES: &[u8] = br#"{"error":"rpc internal serialization failure"}"#;

/// Serialize `resp` and write it (plus trailing newline) to `w`. On
/// serialization failure writes [`FALLBACK_RESPONSE_BYTES`] instead so the
/// script always sees a response and doesn't hang on `sock.recv()`.
pub(crate) async fn write_response<W>(w: &mut W, resp: &RpcResponse) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let bytes = serde_json::to_vec(resp).unwrap_or_else(|_| FALLBACK_RESPONSE_BYTES.to_vec());
    w.write_all(&bytes).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

/// Write `resp` and cleanly shut down the write half.
///
/// Composes [`write_response`] with an always-run `shutdown()` so the
/// client's `recv()` sees EOF promptly and never hangs on Drop timing.
/// The returned `io::Result` reflects only the WRITE outcome — shutdown
/// failures are logged at `trace` and deliberately do not propagate
/// (peer-already-closed is common and benign).
///
/// Return value the caller should hand back from [`handle_rpc_connection`]:
/// - `Ok(())` → `RpcOutcome::Ok`
/// - `Err(_)` → bridge should report `RpcOutcome::IoError`
pub(crate) async fn reply_and_shutdown<W>(w: &mut W, resp: &RpcResponse) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let write_result = write_response(w, resp).await;
    if let Err(e) = w.shutdown().await {
        tracing::trace!(target: "astra_tools::rpc_bridge", error = %e, "writer shutdown failed");
    }
    write_result
}

// ─── Truncation / policy ──────────────────────────────────────────────────

/// Head portion of any head+tail truncation (40%).
pub(crate) const STDOUT_HEAD_RATIO: f64 = 0.4;

/// UTF-8-safe boundary walk (floor): returns the greatest char-boundary ≤ `idx`.
pub(crate) fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// UTF-8-safe boundary walk (ceil): returns the smallest char-boundary ≥ `idx`.
pub(crate) fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Truncate `raw` keeping head (40%) and tail (60%) with a notice in between.
/// UTF-8 safe. Zero `max_bytes` yields an all-omitted notice.
pub fn truncate_head_tail(raw: &str, max_bytes: usize) -> String {
    if raw.len() <= max_bytes {
        return raw.to_string();
    }
    if max_bytes == 0 {
        return format!(
            "... [OUTPUT TRUNCATED — {} bytes omitted out of {} total] ...",
            raw.len(),
            raw.len()
        );
    }

    let head_target = (max_bytes as f64 * STDOUT_HEAD_RATIO) as usize;
    let tail_target = max_bytes.saturating_sub(head_target);

    let head_end = floor_char_boundary(raw, head_target);
    let tail_start = ceil_char_boundary(raw, raw.len().saturating_sub(tail_target));

    let head = &raw[..head_end];
    let tail = &raw[tail_start..];
    let omitted = raw.len().saturating_sub(head.len() + tail.len());

    format!(
        "{head}\n\n... [OUTPUT TRUNCATED — {omitted} bytes omitted out of {} total] ...\n\n{tail}",
        raw.len()
    )
}

/// Apply the non-owning RPC presentation boundary before any response window
/// is selected.  The RPC server may receive a complete result from an agent
/// tool, but it does not own the source bytes and therefore must not mint an
/// edit capability.  Sanitizing first is essential: slicing a credential at
/// the head/tail boundary can turn it into a fragment that no matcher can
/// recognise later.
pub(crate) fn redact_then_truncate_rpc_output(raw: &str, max_bytes: usize) -> String {
    let (safe, _) = crate::credential_redaction::redact_credentials_for_display(raw);
    crate::credential_redaction::truncate_redacted_head_tail(&safe, max_bytes)
}

/// Per-invocation RPC server policy. Caller constructs once per script run.
#[derive(Debug, Clone)]
pub(crate) struct RpcPolicy {
    /// Tools the script is permitted to invoke. Anything else → rejection.
    pub allowed_tools: HashSet<String>,
    /// Maximum number of tool calls allowed in this run.
    pub max_tool_calls: usize,
    /// Maximum size of any single RPC response. Larger responses are
    /// truncated server-side via head+tail before writing.
    pub max_response_bytes: usize,
}

/// Outcome of [`handle_rpc_connection`].
///
/// I/O errors are logged at `debug` inside the bridge and collapsed to
/// `IoError` (unit variant) — the main loop never inspects the error
/// payload, and `std::io::Error` is neither `Clone` nor `PartialEq`, which
/// makes pattern matching on the enum noisy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcOutcome {
    /// Request handled successfully (including policy-rejected requests that
    /// produced error responses).
    Ok,
    /// The script exceeded `max_tool_calls`. Caller should kill the child.
    ExceededCallLimit,
    /// The parent invocation was cancelled while this connection was being
    /// framed, dispatched, or replied to. The run-script owner must terminate
    /// the child before releasing its writer epoch.
    Cancelled,
    /// An unrecoverable I/O error occurred reading/writing the socket.
    /// The RPC server loop should continue accepting new connections —
    /// the child's own `recv()` returns EOF and the script fails fast.
    IoError,
}

/// Write `resp` + shutdown, reporting the outcome uniformly. Swallows
/// the write error and returns `RpcOutcome::IoError` after trace-logging.
async fn reply<W>(w: &mut W, resp: RpcResponse) -> RpcOutcome
where
    W: AsyncWriteExt + Unpin,
{
    match reply_and_shutdown(w, &resp).await {
        Ok(()) => RpcOutcome::Ok,
        Err(e) => {
            tracing::debug!(
                target: "astra_tools::rpc_bridge",
                error = %e,
                "reply write failed"
            );
            RpcOutcome::IoError
        }
    }
}

/// Cancellation-aware response path. A stalled client must not pin the
/// run-script accept loop after its parent has cancelled. Dropping this
/// socket write is safe; the run-script owner still owns child cleanup.
async fn reply_with_cancel<W>(
    w: &mut W,
    resp: RpcResponse,
    cancel_token: Option<&CancellationToken>,
) -> RpcOutcome
where
    W: AsyncWriteExt + Unpin,
{
    let Some(cancel_token) = cancel_token else {
        return reply(w, resp).await;
    };
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), w.shutdown()).await;
            RpcOutcome::Cancelled
        }
        result = reply_and_shutdown(w, &resp) => match result {
            Ok(()) => RpcOutcome::Ok,
            Err(e) => {
                tracing::debug!(target: "astra_tools::rpc_bridge", error = %e, "reply write failed");
                RpcOutcome::IoError
            }
        }
    }
}

async fn reply_with_outcome_and_cancel<W>(
    w: &mut W,
    resp: RpcResponse,
    outcome: RpcOutcome,
    cancel_token: Option<&CancellationToken>,
) -> RpcOutcome
where
    W: AsyncWriteExt + Unpin,
{
    let Some(cancel_token) = cancel_token else {
        return reply_with_outcome(w, resp, outcome).await;
    };
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), w.shutdown()).await;
            RpcOutcome::Cancelled
        }
        _ = reply_and_shutdown(w, &resp) => outcome,
    }
}

/// Same as `reply` but returns a caller-supplied outcome regardless of
/// write success. Used for terminal outcomes like `ExceededCallLimit`
/// where the bridge must signal the main loop to kill the child even if
/// the final response write fails (the script is dead either way).
async fn reply_with_outcome<W>(w: &mut W, resp: RpcResponse, outcome: RpcOutcome) -> RpcOutcome
where
    W: AsyncWriteExt + Unpin,
{
    if let Err(e) = reply_and_shutdown(w, &resp).await {
        tracing::debug!(
            target: "astra_tools::rpc_bridge",
            error = %e,
            "reply_with_outcome write failed"
        );
    }
    outcome
}

/// Handle a single RPC connection: read one JSON-line request, enforce
/// policy, dispatch through `tool_executor`, write the response back.
///
/// Never hangs the script: every code path either writes a response or
/// returns [`RpcOutcome::IoError`] (in which case the script's own socket
/// `recv` returns EOF and the script fails fast).
#[allow(dead_code)]
pub(crate) async fn handle_rpc_connection(
    stream: tokio::net::UnixStream,
    tool_executor: &dyn ToolExecutor,
    call_count: &AtomicUsize,
    policy: &RpcPolicy,
    auth_token: &AuthToken,
) -> RpcOutcome {
    handle_rpc_connection_with_cancel(stream, tool_executor, call_count, policy, auth_token, None)
        .await
}

/// Cancellation-aware variant used by `run_script`.  Keeping the legacy
/// wrapper above preserves the ordinary RPC contract and test helpers while
/// ensuring a cancelled parent invocation reaches the actual nested tool
/// execution instead of merely killing the Python client after the RPC call
/// has already started.
pub(crate) async fn handle_rpc_connection_with_cancel(
    stream: tokio::net::UnixStream,
    tool_executor: &dyn ToolExecutor,
    call_count: &AtomicUsize,
    policy: &RpcPolicy,
    auth_token: &AuthToken,
    cancel_token: Option<&CancellationToken>,
) -> RpcOutcome {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader.take(MAX_RPC_REQUEST_BYTES));
    let mut line = String::new();

    let read_result = if let Some(cancel_token) = cancel_token {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return RpcOutcome::Cancelled,
            result = buf_reader.read_line(&mut line) => result,
        }
    } else {
        buf_reader.read_line(&mut line).await
    };
    if let Err(e) = read_result {
        tracing::debug!(target: "astra_tools::rpc_bridge", error = %e, "read_line failed");
        // Shutdown so the client sees EOF promptly.
        let _ = writer.shutdown().await;
        return RpcOutcome::IoError;
    }
    let line = line.trim();

    if line.is_empty() {
        return reply_with_cancel(
            &mut writer,
            RpcResponse::error("Empty request".into()),
            cancel_token,
        )
        .await;
    }

    let request: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return reply_with_cancel(
                &mut writer,
                RpcResponse::error(format!("Invalid JSON: {e}")),
                cancel_token,
            )
            .await;
        }
    };

    // Auth check. LLM-controlled content → log at debug, not warn.
    if !request.auth_token.constant_time_eq(auth_token) {
        tracing::debug!(
            target: "astra_tools::rpc_bridge",
            tool_name_len = request.tool.len(),
            "rejected RPC request with bad auth_token"
        );
        return reply_with_cancel(
            &mut writer,
            RpcResponse::error("RPC auth failed — invalid auth_token".into()),
            cancel_token,
        )
        .await;
    }

    // Allowlist check.
    if !policy.allowed_tools.contains(&request.tool) {
        return reply_with_cancel(
            &mut writer,
            RpcResponse::error(format!(
                "Tool '{}' is not allowed in run_script sandbox",
                sanitize_for_message(&request.tool)
            )),
            cancel_token,
        )
        .await;
    }

    // Call-count check (pre-increment).
    let count = call_count.fetch_add(1, Ordering::SeqCst) + 1;
    if count > policy.max_tool_calls {
        return reply_with_outcome_and_cancel(
            &mut writer,
            RpcResponse::error(format!(
                "Exceeded maximum tool call limit ({})",
                policy.max_tool_calls
            )),
            RpcOutcome::ExceededCallLimit,
            cancel_token,
        )
        .await;
    }

    // Dispatch through the real executor so tool_health/dedup/compression apply.
    let result = RUN_SCRIPT_RPC_DISPATCH
        .scope(
            (),
            tool_executor.execute_with_cancel(&request.tool, &request.args, cancel_token),
        )
        .await;

    // Cap response size BEFORE serializing — prevents an unbounded tool
    // result from flooding the sandbox and triggering an OOM inside it.
    let capped = redact_then_truncate_rpc_output(&result.output, policy.max_response_bytes);

    let response = if result.is_error {
        RpcResponse::error(capped)
    } else {
        RpcResponse::success(capped)
    };

    reply_with_cancel(&mut writer, response, cancel_token).await
}

/// Strip newlines from a string before embedding in an error message.
/// Prevents LLM-controlled content from forging extra log lines downstream.
fn sanitize_for_message(s: &str) -> String {
    // Hard cap so an arbitrarily long tool name doesn't inflate logs.
    s.chars()
        .filter(|c| *c != '\n' && *c != '\r' && *c != '\0')
        .take(128)
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecutor, ToolResult};
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use tokio::net::UnixListener;

    // ── AuthToken ────────────────────────────────���───────────────────────

    #[test]
    fn auth_token_cases() {
        // random per call
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a.as_str(), b.as_str());
        assert!(a.as_str().len() >= 16);

        // constant-time equality — same-length mismatch
        let a = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = AuthToken::from_str_for_test("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let c = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab");
        assert!(!a.constant_time_eq(&b));
        assert!(!a.constant_time_eq(&c));
        assert!(a.constant_time_eq(&a.clone()));

        // length mismatch rejected
        let short = AuthToken::from_str_for_test("short");
        let long = AuthToken::from_str_for_test("much-longer-token");
        assert!(!short.constant_time_eq(&long));

        // debug redacts value
        let t = AuthToken::from_str_for_test("secret-value-do-not-log");
        let s = format!("{t:?}");
        assert!(!s.contains("secret-value"));
        assert!(s.contains("redacted"));
    }

    // ── Truncation ───────────────────────────────────────────────────────

    #[test]
    fn truncate_no_op_cases() {
        // within limit
        assert_eq!(truncate_head_tail("hello", 100), "hello");
        // exactly at limit
        assert_eq!(truncate_head_tail("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_triggered_cases() {
        // just over max
        let r = truncate_head_tail("abcdef", 5);
        assert!(r.contains("OUTPUT TRUNCATED"), "got: {r}");

        // zero max bytes
        let r = truncate_head_tail("anything goes here", 0);
        assert!(r.contains("OUTPUT TRUNCATED"));
        assert!(r.contains("18 bytes omitted"));

        // max_bytes=1 does not panic
        let r = truncate_head_tail("abcdef", 1);
        assert!(r.contains("OUTPUT TRUNCATED"));
    }

    #[test]
    fn truncate_utf8_boundary_cases() {
        // multi-byte char at split boundary
        let ascii = "abcdefghijklmno"; // 15 bytes
        let cn = "中"; // 3-byte char starting at byte 15
        let body = "这是一段用来测试截断边界的中文内容,包含多字节字符,足够长以触发截断。";
        let input = format!("{ascii}{cn}{body}");
        let result = truncate_head_tail(&input, 40);
        assert!(result.contains("OUTPUT TRUNCATED"));
        assert!(!result.contains('\u{FFFD}'), "utf8 mangled: {result}");

        // emoji boundary
        let input = "prefix🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉suffix";
        let r = truncate_head_tail(input, 30);
        assert!(!r.contains('\u{FFFD}'), "emoji mangled: {r}");
    }

    #[test]
    fn rpc_redacts_before_head_tail_window() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let raw = format!("head {secret} tail {}", "x".repeat(128));
        let output = redact_then_truncate_rpc_output(&raw, 32);
        assert!(
            !output.contains(secret),
            "raw credential crossed RPC window: {output}"
        );
        assert!(output.contains("[REDACTED:AWS_ACCESS_KEY]"));
    }

    // ── Mock executor ────────────────────────────────────────────────────

    struct MockExecutor {
        root: PathBuf,
        call_log: std::sync::Mutex<Vec<(String, Value)>>,
        payload_size: usize,
    }
    impl MockExecutor {
        fn new() -> Self {
            Self {
                root: PathBuf::from("/tmp/test"),
                call_log: std::sync::Mutex::new(Vec::new()),
                payload_size: 0,
            }
        }
        fn with_payload(size: usize) -> Self {
            Self {
                payload_size: size,
                ..Self::new()
            }
        }
        fn call_count(&self) -> usize {
            self.call_log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
        }
    }
    #[async_trait]
    impl ToolExecutor for MockExecutor {
        async fn execute(&self, name: &str, args: &Value) -> ToolResult {
            self.call_log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((name.to_string(), args.clone()));
            if self.payload_size > 0 {
                ToolResult::text("A".repeat(self.payload_size))
            } else {
                ToolResult::text(format!("ok: {name}"))
            }
        }
        fn tool_schemas(&self) -> Vec<Value> {
            vec![]
        }
        fn project_root(&self) -> &Path {
            &self.root
        }
    }

    fn default_policy() -> RpcPolicy {
        RpcPolicy {
            allowed_tools: ["read_file", "grep"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_tool_calls: 50,
            max_response_bytes: 256 * 1024,
        }
    }

    async fn rpc_roundtrip(
        req_json: &str,
        policy: &RpcPolicy,
        token: &AuthToken,
        executor: &dyn ToolExecutor,
        call_count: &AtomicUsize,
    ) -> (String, RpcOutcome) {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (mut reader, mut writer) = client.into_split();
        let (stream, _) = listener.accept().await.unwrap();

        writer.write_all(req_json.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let outcome = handle_rpc_connection(stream, executor, call_count, policy, token).await;

        let mut resp = Vec::new();
        let _ = reader.read_to_end(&mut resp).await;
        (String::from_utf8_lossy(&resp).into_owned(), outcome)
    }

    // ── Core RPC behavior ────────────────────────────────────────────────

    #[tokio::test]
    async fn rpc_happy_path_dispatches_and_returns_output() {
        let policy = default_policy();
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(0);
        let req = r#"{"tool":"read_file","args":{"path":"x"},"auth_token":"tok"}"#;
        let (resp, outcome) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(matches!(outcome, RpcOutcome::Ok));
        assert!(resp.contains("ok: read_file"), "resp: {resp}");
        assert_eq!(exec.call_count(), 1);
    }

    #[tokio::test]
    async fn authenticated_nested_writers_reuse_parent_without_minting_receipts() {
        struct CapturingExecutor {
            inner: crate::executor::DefaultToolExecutor,
            metadata: std::sync::Mutex<Option<serde_json::Map<String, Value>>>,
        }

        #[async_trait]
        impl ToolExecutor for CapturingExecutor {
            async fn execute(&self, name: &str, args: &Value) -> ToolResult {
                assert!(
                    is_run_script_rpc_dispatch(),
                    "authenticated RPC dispatch must carry recursive context"
                );
                let result = ToolExecutor::execute(&self.inner, name, args).await;
                *self.metadata.lock().unwrap_or_else(|e| e.into_inner()) = result.metadata.clone();
                result
            }

            fn tool_schemas(&self) -> Vec<Value> {
                ToolExecutor::tool_schemas(&self.inner)
            }

            fn project_root(&self) -> &Path {
                self.inner.project_root()
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let parent_writer = crate::workspace_observation::begin_workspace_writer_with_options(
            temp.path(),
            None,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("outer run_script writer barrier");
        let executor = CapturingExecutor {
            inner: crate::executor::DefaultToolExecutor::new(crate::ToolContext::test(temp.path())),
            metadata: std::sync::Mutex::new(None),
        };
        let policy = RpcPolicy {
            allowed_tools: ["bash", "write_file"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            max_tool_calls: 2,
            max_response_bytes: 16 * 1024,
        };
        let token = AuthToken::from_str_for_test("tok");
        let counter = AtomicUsize::new(0);
        let request =
            r#"{"tool":"bash","args":{"command":"printf nested > nested.txt"},"auth_token":"tok"}"#;

        assert!(!is_run_script_rpc_dispatch());
        let (response, outcome) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rpc_roundtrip(request, &policy, &token, &executor, &counter),
        )
        .await
        .expect("nested Bash must not wait on a shared-to-exclusive upgrade");
        assert!(!is_run_script_rpc_dispatch(), "RPC context must not leak");
        assert!(matches!(outcome, RpcOutcome::Ok), "{response}");
        assert!(temp.path().join("nested.txt").is_file(), "{response}");
        let typed_request = r#"{"tool":"write_file","args":{"path":"typed.txt","content":"nested typed"},"auth_token":"tok"}"#;
        let (typed_response, typed_outcome) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rpc_roundtrip(typed_request, &policy, &token, &executor, &counter),
        )
        .await
        .expect("authenticated typed callback must reuse the parent lease");
        assert!(matches!(typed_outcome, RpcOutcome::Ok), "{typed_response}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("typed.txt")).unwrap(),
            "nested typed\n"
        );
        let metadata = executor.metadata.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            metadata.as_ref().is_none_or(|fields| {
                fields
                    .get(crate::workspace_observation::RECEIPT_FIELD)
                    .is_none()
            }),
            "only the parent may sign after its complete generation settles: {metadata:?}"
        );
        drop(parent_writer);
    }

    #[tokio::test]
    async fn rpc_wrong_auth_rejected_executor_untouched() {
        let policy = default_policy();
        let token = AuthToken::from_str_for_test("real-token");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(0);
        let req = r#"{"tool":"read_file","args":{"path":"x"},"auth_token":"WRONG"}"#;
        let (resp, _) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(resp.contains("auth"), "resp: {resp}");
        assert_eq!(exec.call_count(), 0);
    }

    #[tokio::test]
    async fn rpc_missing_auth_field_fails_parse_not_dispatch() {
        let policy = default_policy();
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(0);
        // No auth_token field → deserialize error before allowlist/executor run.
        let req = r#"{"tool":"read_file","args":{"path":"x"}}"#;
        let (resp, _) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(
            resp.contains("Invalid JSON") || resp.contains("auth"),
            "resp: {resp}"
        );
        assert_eq!(exec.call_count(), 0);
    }

    #[tokio::test]
    async fn rpc_disallowed_tool_rejected() {
        let policy = default_policy();
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(0);
        let req = r#"{"tool":"unknown_tool","args":{},"auth_token":"tok"}"#;
        let (resp, _) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(resp.contains("not allowed"), "resp: {resp}");
        assert_eq!(exec.call_count(), 0);
    }

    // C3: empty tool name
    #[tokio::test]
    async fn rpc_empty_tool_name_rejected() {
        let policy = default_policy();
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(0);
        let req = r#"{"tool":"","args":{},"auth_token":"tok"}"#;
        let (resp, _) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(resp.contains("not allowed"), "resp: {resp}");
        assert_eq!(exec.call_count(), 0);
    }

    #[tokio::test]
    async fn rpc_exceeding_call_limit_signals_outcome() {
        let mut policy = default_policy();
        policy.max_tool_calls = 1;
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::new();
        let counter = AtomicUsize::new(1); // already at the limit
        let req = r#"{"tool":"read_file","args":{"path":"x"},"auth_token":"tok"}"#;
        let (resp, outcome) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(matches!(outcome, RpcOutcome::ExceededCallLimit));
        assert!(resp.contains("limit"), "resp: {resp}");
    }

    // C13: huge tool result gets capped before reaching the client
    #[tokio::test]
    async fn rpc_huge_response_truncated_server_side() {
        let mut policy = default_policy();
        policy.max_response_bytes = 1024;
        let token = AuthToken::from_str_for_test("tok");
        let exec = MockExecutor::with_payload(50_000);
        let counter = AtomicUsize::new(0);
        let req = r#"{"tool":"read_file","args":{},"auth_token":"tok"}"#;
        let (resp, _) = rpc_roundtrip(req, &policy, &token, &exec, &counter).await;
        assert!(
            resp.len() < 3_000,
            "response not capped: {} bytes",
            resp.len()
        );
        assert!(
            resp.contains("OUTPUT TRUNCATED"),
            "resp: {}",
            &resp[..resp.len().min(200)]
        );
    }

    // R2: write_response never hangs — even with a pathological writer.
    #[tokio::test]
    async fn write_response_writes_newline_terminator() {
        let mut buf = Vec::<u8>::new();
        write_response(&mut buf, &RpcResponse::success("hi".into()))
            .await
            .unwrap();
        assert!(buf.ends_with(b"\n"));
        assert!(String::from_utf8_lossy(&buf).contains("\"output\":\"hi\""));
    }

    /// Writer that fails on shutdown but succeeds on write. Used to exercise
    /// the trace-log path without depending on real socket disconnects.
    struct FailOnShutdown {
        buf: Vec<u8>,
    }
    impl tokio::io::AsyncWrite for FailOnShutdown {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.buf.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("simulated shutdown failure")))
        }
    }

    // T52: shutdown failure is survived — the write succeeded, the response
    // still reaches the client, and reply_and_shutdown returns Ok (the
    // write succeeded; shutdown failure is trace-logged, not propagated).
    #[tokio::test]
    async fn reply_and_shutdown_survives_shutdown_failure() {
        let mut w = FailOnShutdown { buf: Vec::new() };
        let result = reply_and_shutdown(&mut w, &RpcResponse::success("payload".into())).await;
        assert!(
            result.is_ok(),
            "write succeeded; shutdown failure shouldn't propagate"
        );
        assert!(w.buf.ends_with(b"\n"));
        assert!(String::from_utf8_lossy(&w.buf).contains("payload"));
    }

    /// Writer that fails on both write and shutdown. Used to verify the
    /// error propagation contract: the caller should see the WRITE error
    /// (not the later shutdown error).
    struct FailOnEverything;
    impl tokio::io::AsyncWrite for FailOnEverything {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("simulated write failure")))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("simulated flush failure")))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("simulated shutdown failure")))
        }
    }

    // T57: when BOTH write and shutdown fail, caller gets the write error
    // (the first, more informative one) — shutdown failure is swallowed
    // at trace level per the documented contract.
    #[tokio::test]
    async fn reply_and_shutdown_reports_write_error_not_shutdown() {
        let mut w = FailOnEverything;
        let result = reply_and_shutdown(&mut w, &RpcResponse::success("payload".into())).await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("simulated write failure"),
            "expected write-error message, got: {err}"
        );
        // Shutdown error must NOT be what the caller sees.
        assert!(
            !err.to_string().contains("shutdown"),
            "caller should see write-error, not shutdown-error: {err}"
        );
    }

    // R10: log-injection defense — newlines in tool name don't propagate to logs.
    #[test]
    fn sanitize_for_message_strips_newlines_and_caps_length() {
        let evil = format!("a\n[INFO] admin-granted\n{}", "x".repeat(500));
        let clean = sanitize_for_message(&evil);
        assert!(!clean.contains('\n'));
        assert!(clean.len() <= 128);
    }
}
