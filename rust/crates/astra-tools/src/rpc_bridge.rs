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

use crate::ToolExecutor;

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
        // SAFETY: pid > 1 (we spawned it), negative pid = send to process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_child: &tokio::process::Child) {}

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
pub(crate) async fn handle_rpc_connection(
    stream: tokio::net::UnixStream,
    tool_executor: &dyn ToolExecutor,
    call_count: &AtomicUsize,
    policy: &RpcPolicy,
    auth_token: &AuthToken,
) -> RpcOutcome {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader.take(MAX_RPC_REQUEST_BYTES));
    let mut line = String::new();

    if let Err(e) = buf_reader.read_line(&mut line).await {
        tracing::debug!(target: "astra_tools::rpc_bridge", error = %e, "read_line failed");
        // Shutdown so the client sees EOF promptly.
        let _ = writer.shutdown().await;
        return RpcOutcome::IoError;
    }
    let line = line.trim();

    if line.is_empty() {
        return reply(&mut writer, RpcResponse::error("Empty request".into())).await;
    }

    let request: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return reply(
                &mut writer,
                RpcResponse::error(format!("Invalid JSON: {e}")),
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
        return reply(
            &mut writer,
            RpcResponse::error("RPC auth failed — invalid auth_token".into()),
        )
        .await;
    }

    // Allowlist check.
    if !policy.allowed_tools.contains(&request.tool) {
        return reply(
            &mut writer,
            RpcResponse::error(format!(
                "Tool '{}' is not allowed in run_script sandbox",
                sanitize_for_message(&request.tool)
            )),
        )
        .await;
    }

    // Call-count check (pre-increment).
    let count = call_count.fetch_add(1, Ordering::SeqCst) + 1;
    if count > policy.max_tool_calls {
        return reply_with_outcome(
            &mut writer,
            RpcResponse::error(format!(
                "Exceeded maximum tool call limit ({})",
                policy.max_tool_calls
            )),
            RpcOutcome::ExceededCallLimit,
        )
        .await;
    }

    // Dispatch through the real executor so tool_health/dedup/compression apply.
    let result = tool_executor.execute(&request.tool, &request.args).await;

    // Cap response size BEFORE serializing — prevents an unbounded tool
    // result from flooding the sandbox and triggering an OOM inside it.
    let capped = if result.output.len() > policy.max_response_bytes {
        truncate_head_tail(&result.output, policy.max_response_bytes)
    } else {
        result.output
    };

    let response = if result.is_error {
        RpcResponse::error(capped)
    } else {
        RpcResponse::success(capped)
    };

    reply(&mut writer, response).await
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

    // ── AuthToken ────────────────────────────────────────────────────────

    #[test]
    fn auth_token_is_random_per_call() {
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert_ne!(a.as_str(), b.as_str());
        assert!(a.as_str().len() >= 16);
    }

    #[test]
    fn auth_token_constant_time_eq_same_length_mismatch() {
        let a = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = AuthToken::from_str_for_test("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let c = AuthToken::from_str_for_test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab");
        assert!(!a.constant_time_eq(&b));
        assert!(!a.constant_time_eq(&c));
        assert!(a.constant_time_eq(&a.clone()));
    }

    #[test]
    fn auth_token_length_mismatch_rejected() {
        let a = AuthToken::from_str_for_test("short");
        let b = AuthToken::from_str_for_test("much-longer-token");
        assert!(!a.constant_time_eq(&b));
    }

    #[test]
    fn auth_token_debug_redacts_value() {
        let t = AuthToken::from_str_for_test("secret-value-do-not-log");
        let s = format!("{t:?}");
        assert!(!s.contains("secret-value"));
        assert!(s.contains("redacted"));
    }

    // ── Truncation ───────────────────────────────────────────────────────

    #[test]
    fn truncate_no_op_when_within_limit() {
        assert_eq!(truncate_head_tail("hello", 100), "hello");
    }

    // T40: raw.len() exactly equals max_bytes — return unchanged, no notice.
    #[test]
    fn truncate_exact_max_bytes_is_no_op() {
        let s = "abcde"; // 5 bytes
        assert_eq!(truncate_head_tail(s, 5), s);
    }

    // max_bytes + 1 — truncation MUST fire.
    #[test]
    fn truncate_just_over_max_bytes_triggers_notice() {
        let s = "abcdef"; // 6 bytes
        let r = truncate_head_tail(s, 5);
        assert!(r.contains("OUTPUT TRUNCATED"), "got: {r}");
    }

    #[test]
    fn truncate_utf8_safe_at_boundary() {
        let ascii = "abcdefghijklmno"; // 15 bytes
        let cn = "中"; // 3-byte char starting at byte 15
        let body = "这是一段用来测试截断边界的中文内容,包含多字节字符,足够长以触发截断。";
        let input = format!("{ascii}{cn}{body}");
        let result = truncate_head_tail(&input, 40);
        assert!(result.contains("OUTPUT TRUNCATED"));
        assert!(!result.contains('\u{FFFD}'), "utf8 mangled: {result}");
    }

    #[test]
    fn truncate_zero_max_bytes() {
        let r = truncate_head_tail("anything goes here", 0);
        assert!(r.contains("OUTPUT TRUNCATED"));
        assert!(r.contains("18 bytes omitted"));
    }

    #[test]
    fn truncate_max_bytes_one_does_not_panic() {
        let r = truncate_head_tail("abcdef", 1);
        assert!(r.contains("OUTPUT TRUNCATED"));
    }

    #[test]
    fn truncate_emoji_boundary() {
        let input = "prefix🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉suffix";
        let r = truncate_head_tail(input, 30);
        assert!(!r.contains('\u{FFFD}'), "emoji mangled: {r}");
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
            self.call_log.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl ToolExecutor for MockExecutor {
        async fn execute(&self, name: &str, args: &Value) -> ToolResult {
            self.call_log
                .lock()
                .unwrap()
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
        (String::from_utf8_lossy(&resp).to_string(), outcome)
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
        let req = r#"{"tool":"git_commit","args":{},"auth_token":"tok"}"#;
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
