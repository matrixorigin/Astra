//! `run_script` — programmatic tool calling (PTC) via Python + UDS RPC.
//!
//! The LLM writes a Python script; the agent spawns `python3` in a
//! per-run temp directory, exposes a subset of agent tools through a
//! Unix-domain-socket RPC bridge, and returns the script's stdout.
//! Intermediate tool results never enter the LLM context window, so a
//! 10-step multi-tool pipeline collapses into a single inference turn.
//!
//! ### Security posture
//!
//! This module provides **RPC-layer isolation plus optional cgroup v2
//! resource limits**, not a full sandbox:
//!
//! - Per-invocation auth token (128 bits) gates every RPC call.
//! - `env_clear()` + a short, hand-curated env allowlist. No parent env
//!   vars (including secrets like `*_KEY`, `*_TOKEN`) leak to the child.
//! - `HOME` is redirected to the per-run tmpdir by default, so scripts
//!   can't open `~/.ssh/id_rsa` or similar. Opt out via
//!   `RunScriptConfig::isolate_home = false` when a caller legitimately
//!   needs the user's real HOME.
//! - `PATH` is a minimal trusted string in [`ExecutionMode::Strict`] and
//!   passed through in [`ExecutionMode::Project`] (project tooling often
//!   depends on the developer's PATH).
//! - `setsid`'d child so SIGKILL on timeout / call-limit terminates the
//!   entire process group (no orphan grandchildren).
//! - RPC response size is capped server-side — an unbounded tool result
//!   can't OOM the script child.
//! - UTF-8-safe stdout truncation (head + tail).
//! - **cgroup v2 memory + CPU caps** (via `astra_sandbox::apply_cgroup`)
//!   when the host supports it. [`RunScriptConfig::strict_defaults`]
//!   enables 512 MiB memory + 1 CPU core by default; runaway scripts get
//!   a SIGKILL from the kernel OOM killer before they exhaust the host.
//!   Silent fallback on hosts without cgroup v2 write access — the
//!   script still runs, just without resource ceilings.
//!
//! **Not enforced here**: filesystem scope beyond HOME, network access,
//! syscall filtering, PID/mount/network namespaces. The Python script
//! can `open("/etc/passwd")`, `socket.socket()`, and `subprocess.run()`
//! freely. Namespace isolation specifically conflicts with the UDS RPC
//! bridge's `/tmp`-resident socket; callers needing that level of
//! isolation should wrap run_script with their own sandbox at the
//! caller level or use `astra_sandbox::execute_isolated` directly (with
//! the understanding that it doesn't support the RPC channel).
//!
//! ### Differences from legacy `execute_code` (removed)
//!
//! - Dynamic schema filtered by session-enabled tools + priority hint.
//! - Project mode (venv python + session CWD) in addition to strict mode.
//! - Built-in helpers in the stub: `json_parse`, `shell_quote`, `retry`.
//! - Full `ToolExecutor` routing so tool_health / dedup / compression apply.
//! - Response cap, stderr truncation notice, env secret filter, HOME redirect.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

use crate::ToolExecutor;
use crate::rpc_bridge::{
    AuthToken, RpcOutcome, RpcPolicy, STDOUT_HEAD_RATIO, handle_rpc_connection, kill_process_group,
};

// Re-export only what external callers need. The char-boundary helpers are
// implementation details of `truncate_head_tail`; keep them crate-private.
pub use crate::rpc_bridge::truncate_head_tail;

// ─── Constants ─────────────────────────────────────────────────────────────

/// Full set of tools that MAY be invoked by a sandboxed script via RPC.
/// This is *only* an RPC allowlist for agent tools — it does NOT constrain
/// the script's direct filesystem or network access (which are limited by
/// `env_clear`, `isolate_home`, and strict PATH, but not true-sandboxed).
/// Intersected with the caller's `allowed_tools` at schema-build and
/// dispatch time.
pub const RPC_ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "grep",
    "web_fetch",
    "search_files",
    "patch",
    "bash",
];

/// Tools that must NEVER appear in [`RunScriptConfig::strict_defaults`].
///
/// Adding a tool here is the single place to opt it out of the
/// strict-mode allowlist. Any entry here is subtracted from
/// [`RPC_ALLOWED_TOOLS`] when [`RunScriptConfig::strict_defaults`] is
/// called, so future additions to the allowlist don't accidentally land
/// in strict mode.
///
/// Current entry:
/// - `bash`: shell access combined with inherited PATH is
///   indistinguishable from RCE-as-current-user. Block in strict mode.
pub const UNSAFE_IN_STRICT: &[&str] = &["bash"];

/// Default script timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Minimum caller-supplied timeout (prevents `timeout=0` instant-fail DoS).
pub const MIN_TIMEOUT_SECS: u64 = 1;

/// Maximum caller-supplied timeout (10 minutes).
pub const MAX_TIMEOUT_SECS: u64 = 600;

pub const DEFAULT_MAX_TOOL_CALLS: usize = 50;
pub const DEFAULT_MAX_STDOUT_BYTES: usize = 50_000;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1_024;

/// Stderr is capped and annotated — sized to fit a typical Python traceback
/// plus a little room for context lines.
pub const STDERR_CAP_BYTES: usize = 10_000;

// Note: we don't need a secret-substring env filter because we `env_clear()`
// the child and only set a fixed, hand-curated allowlist (see
// `build_child_env`). If the allowlist is ever widened to pass-through
// parent env vars, reinstate a filter here.

// ─── Execution mode ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    /// Scripts run in an isolated temp directory with the system `python3`.
    Strict,
    /// Scripts run in the session's working directory with the active venv's
    /// python. Project deps (pandas, etc.) resolve naturally.
    #[default]
    Project,
}

// ─── Config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RunScriptConfig {
    pub timeout: Duration,
    pub max_tool_calls: usize,
    pub max_stdout_bytes: usize,
    pub max_response_bytes: usize,
    pub allowed_tools: HashSet<String>,
    pub mode: ExecutionMode,
    /// Session working directory (used when mode == Project).
    pub session_cwd: Option<PathBuf>,
    /// If true, redirect `$HOME` in the child env to the run tmpdir. Default
    /// is true (secure-by-default). Disable only when you *need* the real
    /// HOME available and accept the script can read `~/.ssh/*`, etc.
    pub isolate_home: bool,
    /// cgroup v2 memory ceiling for the script process (bytes). Zero
    /// means no limit. Silently falls back to no-limit if cgroup v2 is
    /// unavailable on the host.
    ///
    /// A runaway `run_script` could allocate until the host OOMs
    /// (especially Project mode, which inherits the user's env). Setting
    /// a ceiling turns that into a clean SIGKILL of the child.
    pub memory_limit_bytes: u64,
    /// cgroup v2 CPU quota as a fraction of one core
    /// (e.g. `1.0` = one full core, `0.5` = 50%). Zero means no limit.
    pub cpu_quota: f64,
}

impl Default for RunScriptConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            allowed_tools: RPC_ALLOWED_TOOLS.iter().map(|s| (*s).to_string()).collect(),
            mode: ExecutionMode::default(),
            session_cwd: None,
            isolate_home: true,
            // Default = unlimited, for developer workflows. strict_defaults()
            // opts into real ceilings.
            memory_limit_bytes: 0,
            cpu_quota: 0.0,
        }
    }
}

impl RunScriptConfig {
    /// Build an `RpcPolicy` that mirrors this config's policy-relevant fields.
    fn rpc_policy(&self) -> RpcPolicy {
        RpcPolicy {
            allowed_tools: self.allowed_tools.clone(),
            max_tool_calls: self.max_tool_calls,
            max_response_bytes: self.max_response_bytes,
        }
    }

    /// Tight defaults for untrusted / adversarial scripts:
    /// - `ExecutionMode::Strict` (no venv inheritance, minimal PATH)
    /// - `isolate_home = true`
    /// - `allowed_tools` = [`RPC_ALLOWED_TOOLS`] minus [`UNSAFE_IN_STRICT`]
    /// - `memory_limit_bytes` = 512 MiB (cgroup v2, silent fallback if unavailable)
    /// - `cpu_quota` = 1.0 (one full core)
    ///
    /// Use this constructor when the script source is not trusted (e.g.
    /// evaluation harnesses, benchmark sandboxes, third-party tool execution).
    pub fn strict_defaults() -> Self {
        let allowed_tools = RPC_ALLOWED_TOOLS
            .iter()
            .filter(|name| !UNSAFE_IN_STRICT.contains(name))
            .map(|s| (*s).to_string())
            .collect();
        Self {
            mode: ExecutionMode::Strict,
            isolate_home: true,
            allowed_tools,
            memory_limit_bytes: 512 * 1024 * 1024,
            cpu_quota: 1.0,
            ..Self::default()
        }
    }
}

// ─── Errors ───────────────────────────────────────────────────────────────

/// Maximum characters of stdout / stderr reproduced inline in the
/// `Display` form of [`RunScriptError::ScriptFailed`]. The full streams
/// stay in the struct fields for callers who programmatically inspect
/// them; the `Display` form is what ends up in LLM token budgets and
/// operator logs, so we keep it bounded.
const ERROR_STREAM_PREVIEW_BYTES: usize = 1_000;

#[derive(Debug, Error)]
pub enum RunScriptError {
    #[error("Script timed out after {0:?}")]
    Timeout(Duration),

    #[error("Script exceeded maximum tool call limit ({0})")]
    TooManyToolCalls(usize),

    /// Non-zero exit with neither a timeout nor a syntax error. Both streams
    /// are preserved in the struct fields for programmatic access. The
    /// `Display` impl truncates them to [`ERROR_STREAM_PREVIEW_BYTES`] each
    /// so `{e}` never blows up a token budget — callers that need the
    /// full streams should pattern-match the variant directly.
    #[error(
        "Script exited with code {code}:\nstderr: {}\nstdout: {}",
        preview_stream(stderr),
        preview_stream(stdout)
    )]
    ScriptFailed {
        code: i32,
        stdout: String,
        stderr: String,
    },

    #[error("Script has a syntax error: {0}")]
    SyntaxError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A spawned I/O drainer task (stdout or stderr) failed to join.
    /// This surfaces tokio runtime panics / cancellation and is distinct
    /// from script-level failures so callers can route differently.
    #[error("task-join error: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// Build a bounded preview of a stream for error `Display`. Uses the
/// same UTF-8-safe head+tail truncation the rest of the module uses,
/// so multi-byte chars at the split point never panic or mangle.
fn preview_stream(s: &str) -> String {
    if s.len() <= ERROR_STREAM_PREVIEW_BYTES {
        s.to_string()
    } else {
        truncate_head_tail(s, ERROR_STREAM_PREVIEW_BYTES)
    }
}

// ─── Timeout clamp (R14: testable helper) ─────────────────────────────────

/// Resolve the effective timeout.
///
/// LLM-supplied numeric values are clamped to `[MIN_TIMEOUT_SECS,
/// MAX_TIMEOUT_SECS]`. A caller-supplied `RunScriptConfig::timeout` is
/// trusted and used verbatim if no LLM value is supplied.
///
/// **Numeric inputs (get clamped):**
/// - `3` → 3 seconds
/// - `3.0` / `3.7` → 3 seconds (truncated toward zero)
/// - `0` → clamped up to `MIN_TIMEOUT_SECS`
/// - `3600` → clamped down to `MAX_TIMEOUT_SECS`
/// - `0.4` → truncates to 0, then clamps up to MIN
///
/// **Treated as absent → caller default:**
/// - Missing field
/// - Non-number (`"five"`, `null`, objects, arrays)
/// - Negative (`-1`)
///
/// Non-finite floats (`NaN`, `Infinity`) are also rejected defensively,
/// but in practice serde_json cannot construct such values — the JSON
/// spec forbids them. The defense only fires if a caller builds a
/// `Value` directly.
pub fn resolve_timeout(llm_arg: Option<&Value>, caller_timeout: Duration) -> Duration {
    let secs = llm_arg.and_then(parse_timeout_secs);
    match secs {
        Some(n) => Duration::from_secs(n.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)),
        None => caller_timeout,
    }
}

/// Coerce a JSON timeout value into a non-negative `u64` seconds count.
/// Returns `None` for anything not usable (negative, non-finite, non-number).
///
/// Note: `Some(0)` is a legitimate canonical result here; clamping to
/// `MIN_TIMEOUT_SECS` is the caller's (`resolve_timeout`'s) job so this
/// function stays a pure parse.
fn parse_timeout_secs(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    // Handle JSON float. `as_f64` succeeds for integer JSON numbers too,
    // but we try `as_u64` first for precision.
    let f = v.as_f64()?;
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    // Truncate toward zero. 3.7 → 3, 3.0 → 3.
    Some(f as u64)
}

// ─── Python stub generation ───────────────────────────────────────────────

struct ToolStubDef {
    name: &'static str,
    signature: &'static str,
    docstring: &'static str,
    args_expr: &'static str,
}

const TOOL_STUB_DEFS: &[ToolStubDef] = &[
    ToolStubDef {
        name: "read_file",
        signature: "path, offset=None, limit=None",
        docstring: "Read a file's contents. Returns the file text.",
        args_expr: r#"{"path": path, "offset": offset, "limit": limit}"#,
    },
    ToolStubDef {
        name: "write_file",
        signature: "path, content",
        docstring: "Write content to a file (overwrites).",
        args_expr: r#"{"path": path, "content": content}"#,
    },
    ToolStubDef {
        name: "list_dir",
        signature: "path='.'",
        docstring: "List directory contents.",
        args_expr: r#"{"path": path}"#,
    },
    ToolStubDef {
        name: "grep",
        signature: "pattern, path=None, include=None",
        docstring: "Search for a pattern in files.",
        args_expr: r#"{"pattern": pattern, "path": path, "include": include}"#,
    },
    ToolStubDef {
        name: "web_fetch",
        signature: "url, format='markdown'",
        docstring: "Fetch a URL and return its content.",
        args_expr: r#"{"url": url, "format": format}"#,
    },
    ToolStubDef {
        name: "search_files",
        signature: "pattern, target='content', path='.', file_glob=None, limit=50",
        docstring: "Search file contents or find files by name.",
        args_expr: r#"{"pattern": pattern, "target": target, "path": path, "file_glob": file_glob, "limit": limit}"#,
    },
    ToolStubDef {
        name: "patch",
        signature: "path, old_string, new_string, replace_all=False",
        docstring: "Find-and-replace in a file.",
        args_expr: r#"{"path": path, "old_string": old_string, "new_string": new_string, "replace_all": replace_all}"#,
    },
    ToolStubDef {
        name: "bash",
        signature: "command, timeout=None",
        docstring: "Run a shell command (subject to shell hardening). Returns {output, exit_code}.",
        args_expr: r#"{"command": command, "timeout": timeout}"#,
    },
];

struct ToolDocLine {
    name: &'static str,
    doc: &'static str,
}

const TOOL_DOC_LINES: &[ToolDocLine] = &[
    ToolDocLine {
        name: "read_file",
        doc: "  read_file(path, offset=None, limit=None) — read file contents",
    },
    ToolDocLine {
        name: "write_file",
        doc: "  write_file(path, content) — write/overwrite a file",
    },
    ToolDocLine {
        name: "list_dir",
        doc: "  list_dir(path='.') — list directory",
    },
    ToolDocLine {
        name: "grep",
        doc: "  grep(pattern, path=None, include=None) — search files",
    },
    ToolDocLine {
        name: "web_fetch",
        doc: "  web_fetch(url, format='markdown') — fetch URL content",
    },
    ToolDocLine {
        name: "search_files",
        doc: "  search_files(pattern, target='content', path='.', file_glob=None, limit=50) — search/find files",
    },
    ToolDocLine {
        name: "patch",
        doc: "  patch(path, old_string, new_string, replace_all=False) — find-and-replace",
    },
    ToolDocLine {
        name: "bash",
        doc: "  bash(command, timeout=None) — shell command (hardened)",
    },
];

const BUILTIN_HELPERS: &str = r#"

# ─── Built-in helpers ──────────────────────────────────────────────────────

def json_parse(text):
    """Parse JSON tolerant of control characters (strict=False).
    Use instead of json.loads() when parsing output from bash() or web_fetch()
    that may contain raw tabs/newlines in strings."""
    return json.loads(text, strict=False)


def shell_quote(s):
    """Shell-escape a string for safe interpolation into commands."""
    import shlex
    return shlex.quote(s)


def retry(fn, max_attempts=3, delay=2):
    """Retry a function up to max_attempts times with exponential backoff."""
    import time as _time
    last_err = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            if attempt < max_attempts - 1:
                _time.sleep(delay * (2 ** attempt))
    raise last_err
"#;

/// Generate the `astra_tools.py` stub module for the given set of enabled tools.
pub(crate) fn generate_python_stub(enabled_tools: &HashSet<String>) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(
        r#""""
astra_tools — RPC bridge for calling agent tools from Python scripts.
Auto-generated. Do not edit.
"""
import json
import socket
import os

_SOCKET_PATH = os.environ["ASTRA_RPC_SOCKET"]
_AUTH_TOKEN = os.environ["ASTRA_RPC_AUTH_TOKEN"]


def _call(tool_name, args):
    """Send an RPC request to the agent and return the result.

    Uses a fresh socket per call so disconnection mid-request does not
    corrupt a shared connection. The try/finally guarantees the socket
    is closed even if sendall/recv raises, preventing fd leaks.
    """
    # Strip None values to keep payloads clean
    clean_args = {k: v for k, v in args.items() if v is not None}
    request = json.dumps({
        "tool": tool_name,
        "args": clean_args,
        "auth_token": _AUTH_TOKEN,
    })
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(_SOCKET_PATH)
        sock.settimeout(300)
        sock.sendall((request + "\n").encode())
        sock.shutdown(socket.SHUT_WR)
        buf = b""
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            buf += chunk
    finally:
        sock.close()
    raw = buf.decode().strip()
    result = json.loads(raw)
    if result.get("error"):
        raise RuntimeError(result["error"])
    return result["output"]

"#,
    );

    for def in TOOL_STUB_DEFS {
        if !enabled_tools.contains(def.name) {
            continue;
        }
        // Use JSON encoding for the literal tool name so the generated
        // Python is guaranteed valid even for names containing quotes,
        // backslashes, or non-ASCII chars. Rust's `{:?}` is debug-syntax,
        // not a guaranteed-valid Python literal.
        let name_literal = serde_json::to_string(def.name)
            .expect("static &str always serializes to a JSON string");
        out.push_str(&format!(
            "\ndef {}({}):\n    \"\"\"{}\"\"\"\n    return _call({}, {})\n",
            def.name, def.signature, def.docstring, name_literal, def.args_expr
        ));
    }

    out.push_str(BUILTIN_HELPERS);
    out
}

// ─── Dynamic schema builder ───────────────────────────────────────────────

/// Session-level priority signal injected into the `run_script` schema
/// description. Lets the session state steer the model toward or away
/// from this tool without changing its availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriorityHint {
    /// No bias. Schema description lists the available tools and built-in
    /// helpers without additional steering. This is the default.
    #[default]
    Neutral,
    /// Session prefers run_script for multi-step tasks. The schema
    /// description gains a `**PREFERRED**` marker so the model biases
    /// toward it for 3+ tool-call pipelines.
    Pinned,
    /// Session has found run_script unhelpful / noisy. The schema
    /// description gains a `**DEPRIORITIZED**` marker so the model
    /// prefers individual tool calls.
    Deprioritized,
}

pub fn build_run_script_schema(
    enabled_tools: &HashSet<String>,
    mode: ExecutionMode,
    priority: PriorityHint,
) -> Value {
    let tool_lines: String = TOOL_DOC_LINES
        .iter()
        .filter(|t| enabled_tools.contains(t.name))
        .map(|t| t.doc)
        .collect::<Vec<_>>()
        .join("\n");

    let mode_note = match mode {
        ExecutionMode::Project => {
            "Scripts run in the session's working directory with the active venv's python, \
             so project deps and relative paths work naturally."
        }
        ExecutionMode::Strict => {
            "Scripts run in an isolated temp directory — use absolute paths or tool calls \
             for file access."
        }
    };

    let priority_note = match priority {
        PriorityHint::Pinned => {
            "\n\n**PREFERRED**: This tool is pinned as the preferred approach for multi-step tasks \
             in this session. Use it when you need 3+ tool calls with processing logic between them."
        }
        PriorityHint::Deprioritized => {
            "\n\n**DEPRIORITIZED**: Prefer individual tool calls unless you specifically need \
             batch processing with conditional logic."
        }
        PriorityHint::Neutral => "",
    };

    let import_examples: Vec<&str> = ["read_file", "bash", "web_fetch"]
        .iter()
        .filter(|n| enabled_tools.contains(**n))
        .copied()
        .collect();

    // Empty enabled_tools → degrade gracefully rather than emitting broken Python hints.
    let available_block = if tool_lines.is_empty() {
        "No sandbox tools are enabled for this session. run_script can only \
         execute pure Python (stdlib) without calling any agent tool."
            .to_string()
    } else {
        let example = if import_examples.is_empty() {
            let mut names: Vec<_> = enabled_tools.iter().take(2).cloned().collect();
            names.sort();
            names.join(", ")
        } else {
            import_examples.join(", ")
        };
        format!("Available via `from astra_tools import {example}, ...`:\n\n{tool_lines}")
    };

    let script_param_desc = if tool_lines.is_empty() {
        "Python code to execute. No tool bindings are available this session — \
         use only Python stdlib. Print your final result to stdout."
            .to_string()
    } else {
        let example = if import_examples.is_empty() {
            let mut names: Vec<_> = enabled_tools.iter().take(2).cloned().collect();
            names.sort();
            names.join(", ")
        } else {
            import_examples.join(", ")
        };
        format!(
            "Python code to execute. Import tools with \
             `from astra_tools import {example}, ...` and print your \
             final result to stdout."
        )
    };

    let description = format!(
        "Run a Python script that can call agent tools programmatically. \
         Use when you need 3+ tool calls with processing logic between them, \
         need to filter/reduce large outputs before they enter context, \
         need conditional branching, or need to loop.\n\n\
         {available_block}\n\n\
         Limits: 5-minute timeout, 50KB stdout cap, max 50 tool calls per script.\n\n\
         {mode_note}\n\n\
         Also available (built-in, no import needed):\n  \
         json_parse(text) — json.loads with strict=False\n  \
         shell_quote(s) — shlex.quote() for safe shell interpolation\n  \
         retry(fn, max_attempts=3, delay=2) — retry with exponential backoff\
         {priority_note}"
    );

    let timeout_desc = format!(
        "Optional timeout in seconds ({min}–{max}, default {default}).",
        min = MIN_TIMEOUT_SECS,
        max = MAX_TIMEOUT_SECS,
        default = DEFAULT_TIMEOUT.as_secs(),
    );

    serde_json::json!({
        "type": "function",
        "function": {
            "name": "run_script",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "script": {"type": "string", "description": script_param_desc},
                    "timeout": {"type": "integer", "description": timeout_desc},
                },
                "required": ["script"]
            }
        }
    })
}

// ─── Execution mode resolvers ─────────────────────────────────────────────

pub(crate) fn resolve_python(mode: ExecutionMode) -> String {
    if mode == ExecutionMode::Strict {
        return "python3".to_string();
    }
    for var in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Ok(root) = std::env::var(var) {
            let root = root.trim().to_string();
            if root.is_empty() {
                continue;
            }
            let candidate = format!("{root}/bin/python3");
            if Path::new(&candidate).is_file() && is_usable_python(&candidate) {
                return candidate;
            }
            let candidate2 = format!("{root}/bin/python");
            if Path::new(&candidate2).is_file() && is_usable_python(&candidate2) {
                return candidate2;
            }
        }
    }
    "python3".to_string()
}

/// Resolve the working directory for the child subprocess.
///
/// Contract:
/// - `ExecutionMode::Strict`: always `fallback` (the per-run tmpdir).
/// - `ExecutionMode::Project`: if `session_cwd` is `Some` and points at a
///   real directory, use it. Otherwise `fallback`. We intentionally do NOT
///   fall back to `std::env::current_dir()`: the process CWD at call time
///   is coincidental state, and using it breaks the per-run isolation
///   guarantee callers rely on.
///
/// The caller is responsible for ensuring `fallback` actually exists
/// before spawning. The public `run_script` entry point always passes
/// its freshly-created tmpdir, so the guarantee holds in practice.
pub(crate) fn resolve_cwd(
    mode: ExecutionMode,
    session_cwd: Option<&Path>,
    fallback: &Path,
) -> PathBuf {
    if mode == ExecutionMode::Strict {
        return fallback.to_path_buf();
    }
    if let Some(cwd) = session_cwd.filter(|p| p.is_dir()) {
        return cwd.to_path_buf();
    }
    fallback.to_path_buf()
}

/// Maximum entries kept in [`PYTHON_VERSION_CACHE`]. Long-running servers
/// that cycle through many venvs should not balloon memory forever.
///
/// Insertion policy:
/// 1. A stale entry for the same `path` is removed first (via dedup) so
///    re-inserting never double-counts.
/// 2. Only after dedup do we check capacity — if the cache is still at
///    cap, the oldest-half of entries is evicted FIFO.
/// 3. The fresh entry is appended at the end (most-recent position).
///
/// 64 is generous for typical deployments (the entries are "python3" +
/// per-venv python; both stable for the process lifetime).
const PYTHON_CACHE_CAP: usize = 64;

/// Fingerprint of a Python binary for cache-validity checking.
///
/// Combines `mtime` + `len` because most Linux filesystems expose
/// mtime at second resolution — a venv rebuild that completes within
/// the same second would otherwise appear unchanged. File size changes
/// for any rebuild that produces a different binary, catching the gap.
///
/// Either field is `None` when the filesystem metadata call fails
/// (nonexistent path, permission denied, unsupported on some FUSE fs,
/// etc.). Two such `None` fingerprints compare equal by design, so an
/// unstat-able path doesn't thrash the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PythonFingerprint {
    mtime: Option<std::time::SystemTime>,
    len: Option<u64>,
}

impl PythonFingerprint {
    fn for_path(path: &str) -> Self {
        match std::fs::metadata(path) {
            Ok(md) => Self {
                mtime: md.modified().ok(),
                len: Some(md.len()),
            },
            Err(_) => Self {
                mtime: None,
                len: None,
            },
        }
    }
}

/// Cache for `is_usable_python` probe results. Keyed by (path,
/// fingerprint) so rebuilt venvs invalidate their entries automatically:
/// if `/venv/bin/python3`'s mtime changes, the old cache entry no
/// longer matches and we re-probe.
///
/// Uses a `Vec` to preserve insertion order cheaply. At typical sizes
/// (≤ a handful), linear lookup is faster than HashMap hashing.
static PYTHON_VERSION_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Vec<(String, PythonFingerprint, bool)>>,
> = std::sync::OnceLock::new();

fn python_cache() -> &'static std::sync::Mutex<Vec<(String, PythonFingerprint, bool)>> {
    PYTHON_VERSION_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Clear the version-probe cache. Test-only.
#[cfg(test)]
pub(crate) fn clear_python_cache() {
    python_cache().lock().unwrap().clear();
}

/// Check if a Python interpreter is usable (>= 3.8). Cached per
/// (path, mtime), bounded to [`PYTHON_CACHE_CAP`] entries.
///
/// Cache entries are invalidated when the file's mtime changes — so a
/// replaced/rebuilt venv triggers a re-probe on the next call.
fn is_usable_python(path: &str) -> bool {
    let fingerprint = PythonFingerprint::for_path(path);
    {
        let cache = python_cache().lock().unwrap();
        if let Some((_, _, hit)) = cache
            .iter()
            .find(|(k, fp, _)| k == path && *fp == fingerprint)
        {
            return *hit;
        }
    }

    let ok = std::process::Command::new(path)
        .args([
            "-c",
            "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let mut cache = python_cache().lock().unwrap();
    // Remove any stale entry for this path (different fingerprint) — we
    // just learned the up-to-date answer, keep only one entry per path.
    cache.retain(|(k, _, _)| k != path);
    // Evict oldest half when at cap — FIFO-ish, cheap, bounded memory.
    if cache.len() >= PYTHON_CACHE_CAP {
        let drop_n = PYTHON_CACHE_CAP / 2;
        cache.drain(..drop_n);
    }
    cache.push((path.to_string(), fingerprint, ok));
    ok
}

/// Report whether `python3` is usable on PATH. Used by ignored integration
/// tests to skip when the host doesn't have Python.
pub fn python3_available() -> bool {
    is_usable_python("python3")
}

// ─── Main execution ───────────────────────────────────────────────────────

/// Wrap an `io::Error` with a short site-of-failure tag so bubbled-up
/// errors pinpoint which setup step failed (tmpdir creation vs write vs
/// bind vs spawn). Cheap: one allocation per error path, only on error.
fn io_context(ctx: &'static str, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{ctx}: {e}"))
}

pub async fn run_script(
    script: &str,
    config: &RunScriptConfig,
    tool_executor: &dyn ToolExecutor,
) -> Result<String, RunScriptError> {
    let tmp_dir = tempfile::tempdir().map_err(|e| io_context("run_script tmpdir", e))?;
    let tmp_path = tmp_dir.path().to_path_buf();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp_path)
            .map_err(|e| io_context("run_script tmpdir metadata", e))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&tmp_path, perms)
            .map_err(|e| io_context("run_script tmpdir chmod 0o700", e))?;
    }

    let socket_path = tmp_path.join("rpc.sock");
    let stub_path = tmp_path.join("astra_tools.py");
    let script_path = tmp_path.join("script.py");

    let auth_token = AuthToken::generate();

    std::fs::write(&stub_path, generate_python_stub(&config.allowed_tools))
        .map_err(|e| io_context("run_script write astra_tools.py", e))?;
    std::fs::write(&script_path, script)
        .map_err(|e| io_context("run_script write script.py", e))?;

    let listener =
        UnixListener::bind(&socket_path).map_err(|e| io_context("run_script UDS bind", e))?;
    let call_count = Arc::new(AtomicUsize::new(0));

    let python = resolve_python(config.mode);
    let cwd = resolve_cwd(config.mode, config.session_cwd.as_deref(), &tmp_path);

    let env_pairs = build_child_env(
        &tmp_path,
        &socket_path,
        &auth_token,
        config.mode,
        config.isolate_home,
    );

    let mut cmd = Command::new(&python);
    cmd.arg(&script_path).current_dir(&cwd).env_clear();
    for (k, v) in &env_pairs {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Attach cgroup v2 limits before spawn. The guard must stay alive
    // until the child has exited and been waited on — otherwise Drop
    // tries to remove a non-empty cgroup directory. Silent fallback when
    // cgroup v2 is unavailable: the guard is inactive and does nothing.
    let _cgroup_guard =
        astra_sandbox::apply_cgroup(&mut cmd, config.memory_limit_bytes, config.cpu_quota);

    let mut child = cmd
        .spawn()
        .map_err(|e| io_context("run_script spawn python", e))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let max_stdout = config.max_stdout_bytes;
    let stdout_handle =
        tokio::spawn(async move { collect_stdout_head_tail(stdout, max_stdout).await });

    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_handle = tokio::spawn(async move { collect_stderr_with_notice(stderr).await });

    let policy = config.rpc_policy();
    let timeout_result = tokio::time::timeout(config.timeout, async {
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let outcome = handle_rpc_connection(
                                stream,
                                tool_executor,
                                &call_count,
                                &policy,
                                &auth_token,
                            ).await;
                            if matches!(outcome, RpcOutcome::ExceededCallLimit) {
                                kill_process_group(&child);
                                let _ = child.kill().await;
                                return child.wait().await;
                            }
                            // IoError and Ok both just loop.
                        }
                        Err(_) => return child.wait().await,
                    }
                }
                wait_result = child.wait() => {
                    return wait_result;
                }
            }
        }
    })
    .await;

    match timeout_result {
        Ok(Ok(status)) => {
            // Await both IO tasks concurrently so one panicking doesn't leak
            // the other. try_join! short-circuits on first Err; the
            // #[from] JoinError impl does the structured wrap.
            let (stdout_content, stderr_content) = tokio::try_join!(stdout_handle, stderr_handle)?;

            if !status.success() {
                let code = status.code().unwrap_or(-1);
                if stderr_content.contains("SyntaxError") {
                    return Err(RunScriptError::SyntaxError(stderr_content));
                }
                return Err(RunScriptError::ScriptFailed {
                    code,
                    stdout: stdout_content,
                    stderr: stderr_content,
                });
            }

            Ok(stdout_content)
        }
        Ok(Err(e)) => {
            kill_process_group(&child);
            let _ = child.kill().await;
            Err(RunScriptError::Io(e))
        }
        Err(_) => {
            kill_process_group(&child);
            let _ = child.kill().await;
            Err(RunScriptError::Timeout(config.timeout))
        }
    }
}

/// Minimal trusted PATH for `ExecutionMode::Strict`. Avoids inheriting a
/// developer machine's PATH which may contain `aws`, `kubectl`, etc.
const STRICT_PATH: &str = "/usr/bin:/bin";

/// Build the env var pairs to pass to the child. The child starts from
/// `env_clear()`; this helper returns the *exact* set the child will see.
///
/// - `HOME` points at the per-run tmpdir when `isolate_home=true` (default),
///   so scripts can't open `~/.ssh/*`.
/// - `PATH` is a trusted minimal string in Strict mode; Project mode
///   inherits the parent PATH because developers often depend on their
///   personal PATH for project tooling (uv, poetry, etc.).
fn build_child_env(
    tmp_path: &Path,
    socket_path: &Path,
    auth_token: &AuthToken,
    mode: ExecutionMode,
    isolate_home: bool,
) -> Vec<(String, String)> {
    let path = match mode {
        ExecutionMode::Strict => STRICT_PATH.to_string(),
        ExecutionMode::Project => std::env::var("PATH").unwrap_or_else(|_| STRICT_PATH.to_string()),
    };
    let home = if isolate_home {
        tmp_path.display().to_string()
    } else {
        // Unset OR empty HOME → per-run tmpdir (empty HOME breaks
        // os.path.expanduser and tmpdir is safer than a world-writable path).
        std::env::var("HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| tmp_path.display().to_string())
    };

    let mut out: Vec<(String, String)> = vec![
        (
            "ASTRA_RPC_SOCKET".to_string(),
            socket_path.display().to_string(),
        ),
        (
            "ASTRA_RPC_AUTH_TOKEN".to_string(),
            auth_token.as_str().to_string(),
        ),
        ("HOME".to_string(), home),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("PATH".to_string(), path),
        ("PYTHONPATH".to_string(), tmp_path.display().to_string()),
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
    ];
    // Stable ordering for test determinism and easier diffing in logs.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ─── Stdout collector (head + tail, UTF-8 safe) ───────────────────────────

async fn collect_stdout_head_tail(stdout: tokio::process::ChildStdout, max_bytes: usize) -> String {
    if max_bytes == 0 {
        let mut reader = BufReader::new(stdout);
        let mut sink = [0u8; 4096];
        let mut total = 0usize;
        loop {
            match reader.read(&mut sink).await {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        return if total > 0 {
            format!("... [OUTPUT TRUNCATED — {total} bytes omitted out of {total} total] ...")
        } else {
            String::new()
        };
    }

    let head_bytes = (max_bytes as f64 * STDOUT_HEAD_RATIO) as usize;
    let tail_bytes = max_bytes - head_bytes;

    let mut reader = BufReader::new(stdout);
    let mut head_buf = Vec::with_capacity(head_bytes.min(8192));
    let mut tail_ring = Vec::with_capacity(tail_bytes.min(8192));
    let mut total_bytes = 0usize;
    let mut head_full = head_bytes == 0;

    let mut chunk = vec![0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut data = &chunk[..n];
                total_bytes += n;

                if !head_full {
                    let remaining = head_bytes - head_buf.len();
                    if data.len() <= remaining {
                        head_buf.extend_from_slice(data);
                        continue;
                    } else {
                        head_buf.extend_from_slice(&data[..remaining]);
                        head_full = true;
                        data = &data[remaining..];
                        if data.is_empty() {
                            continue;
                        }
                    }
                }

                tail_ring.extend_from_slice(data);
                if tail_ring.len() > tail_bytes {
                    let excess = tail_ring.len() - tail_bytes;
                    tail_ring.drain(..excess);
                }
            }
            Err(_) => break,
        }
    }

    let head = String::from_utf8_lossy(&head_buf).to_string();
    let tail = String::from_utf8_lossy(&tail_ring).to_string();

    if total_bytes > max_bytes && !tail.is_empty() {
        let omitted = total_bytes.saturating_sub(head.len() + tail.len());
        format!(
            "{head}\n\n... [OUTPUT TRUNCATED — {omitted} bytes omitted out of {total_bytes} total] ...\n\n{tail}"
        )
    } else {
        format!("{head}{tail}")
    }
}

/// Drain stderr, keeping up to `STDERR_CAP_BYTES` and appending a truncation
/// notice when more was produced. Always fully drains the pipe so the child
/// never blocks on a full buffer.
async fn collect_stderr_with_notice(stderr: tokio::process::ChildStderr) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut reader = BufReader::new(stderr);
    let mut total = 0usize;
    let mut chunk = vec![0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if buf.len() < STDERR_CAP_BYTES {
                    let take = (STDERR_CAP_BYTES - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    let mut out = String::from_utf8_lossy(&buf).to_string();
    if total > STDERR_CAP_BYTES {
        let omitted = total.saturating_sub(STDERR_CAP_BYTES);
        // Ensure a blank line before the notice — stderr sometimes lacks a
        // trailing newline, and gluing the marker onto the last line is
        // visually confusing. The `[stderr]` tag distinguishes this from
        // the stdout-side truncation notice when both appear in the same
        // ScriptFailed error payload (Display concats both streams).
        out.push_str(&format!(
            "\n\n... [stderr OUTPUT TRUNCATED — {omitted} bytes omitted out of {total} total] ...\n"
        ));
    }
    out
}

// ─── Tool entry point ─────────────────────────────────────────────────────

pub async fn handle_run_script(
    args: &Value,
    tool_executor: &dyn ToolExecutor,
    config: RunScriptConfig,
) -> crate::ToolResult {
    let script = match args.get("script").and_then(Value::as_str) {
        Some(s) => s,
        None => return crate::ToolResult::error("Error: Missing 'script' parameter".into()),
    };

    let timeout = resolve_timeout(args.get("timeout"), config.timeout);
    let config = RunScriptConfig { timeout, ..config };

    match run_script(script, &config, tool_executor).await {
        Ok(output) => {
            if output.is_empty() {
                crate::ToolResult::text("(script completed with no output)".into())
            } else {
                crate::ToolResult::text(output)
            }
        }
        Err(e) => crate::ToolResult::error(format!("Error: {e}")),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolExecutor, ToolResult};
    use async_trait::async_trait;
    use serial_test::serial;

    // ── RAII env guard (R3.7) ────────────────────────────────────────────

    /// Save-and-restore a process env var for the duration of a test.
    /// Restores the prior value (or unsets) on drop — robust against panics.
    /// Pair with `#[serial]` on any test that uses it so
    /// concurrent tests never observe the in-flight mutation.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            // SAFETY: test code; callers are `#[serial]` so no cross-thread
            // reads of this var happen during the guard's lifetime.
            unsafe { std::env::set_var(key, value) };
            Self { key, prior }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: test code; see EnvGuard::set.
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // ── Mock executor ────────────────────────────────────────────────────

    struct MockToolExecutor {
        project_root: PathBuf,
        call_log: std::sync::Mutex<Vec<(String, Value)>>,
    }
    impl MockToolExecutor {
        fn new() -> Self {
            Self {
                project_root: PathBuf::from("/tmp/test"),
                call_log: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.call_log.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl ToolExecutor for MockToolExecutor {
        async fn execute(&self, name: &str, args: &Value) -> ToolResult {
            self.call_log
                .lock()
                .unwrap()
                .push((name.to_string(), args.clone()));
            match name {
                "read_file" => {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("content of {path}"))
                }
                "write_file" => ToolResult::text("File written successfully".into()),
                "list_dir" => ToolResult::text("file1.txt\nfile2.rs\ndir/".into()),
                "grep" => {
                    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                    ToolResult::text(format!("match: {pattern}"))
                }
                _ => ToolResult::error(format!("Unknown tool: {name}")),
            }
        }
        fn tool_schemas(&self) -> Vec<Value> {
            vec![]
        }
        fn project_root(&self) -> &Path {
            &self.project_root
        }
    }

    // ── Config defaults (A3.3: pin secure-by-default choices) ────────────

    #[test]
    fn default_config_isolates_home_for_security() {
        // Regression guard: no future refactor may silently flip this to false.
        assert!(
            RunScriptConfig::default().isolate_home,
            "isolate_home must default to true — scripts should not see real HOME"
        );
    }

    #[test]
    fn default_config_mode_is_project() {
        // Developers running in a venv/conda env expect pandas etc. to resolve.
        assert_eq!(RunScriptConfig::default().mode, ExecutionMode::Project);
    }

    // B2: cgroup fields default to "unlimited" (0) in permissive Project
    // mode — developers running their own code shouldn't hit a surprise
    // 512MB ceiling. strict_defaults() opts into real limits.
    #[test]
    fn default_config_cgroup_fields_are_unlimited() {
        let cfg = RunScriptConfig::default();
        assert_eq!(cfg.memory_limit_bytes, 0, "default should not cap memory");
        assert_eq!(cfg.cpu_quota, 0.0, "default should not cap CPU");
    }

    #[test]
    fn strict_defaults_cgroup_fields_capped() {
        let cfg = RunScriptConfig::strict_defaults();
        assert!(cfg.memory_limit_bytes > 0, "strict must cap memory");
        assert!(cfg.cpu_quota > 0.0, "strict must cap CPU");
        // Concrete anchors: 512 MB memory, 1 core CPU — matches the
        // numbers astra-sandbox uses for IsolationConfig::strict.
        assert_eq!(cfg.memory_limit_bytes, 512 * 1024 * 1024);
        assert_eq!(cfg.cpu_quota, 1.0);
    }

    #[test]
    fn default_config_uses_full_rpc_allowlist() {
        let cfg = RunScriptConfig::default();
        for tool in RPC_ALLOWED_TOOLS {
            assert!(
                cfg.allowed_tools.contains(*tool),
                "{tool} missing from default allowed_tools"
            );
        }
    }

    // S1: strict_defaults() excludes `bash` and picks Strict mode.
    #[test]
    fn strict_defaults_excludes_bash() {
        let cfg = RunScriptConfig::strict_defaults();
        assert_eq!(cfg.mode, ExecutionMode::Strict);
        assert!(cfg.isolate_home, "strict defaults must isolate HOME");
        assert!(
            !cfg.allowed_tools.contains("bash"),
            "strict defaults must NOT allow bash"
        );
        // Everything else from RPC_ALLOWED_TOOLS is still there.
        for tool in RPC_ALLOWED_TOOLS.iter().filter(|t| **t != "bash") {
            assert!(
                cfg.allowed_tools.contains(*tool),
                "strict defaults should still allow {tool}"
            );
        }
    }

    // S3: strict_defaults() excludes **every** name in UNSAFE_IN_STRICT.
    // If a future maintainer adds a new risky tool, adding it to
    // UNSAFE_IN_STRICT alone (single source of truth) keeps strict clean.
    #[test]
    fn strict_defaults_excludes_full_unsafe_list() {
        let cfg = RunScriptConfig::strict_defaults();
        for unsafe_tool in UNSAFE_IN_STRICT {
            assert!(
                !cfg.allowed_tools.contains(*unsafe_tool),
                "strict defaults leaks unsafe tool {unsafe_tool}"
            );
        }
    }

    // S3 (pin): every security-relevant field has a known-safe value.
    // If a future field is added without a secure default, this test
    // catches the omission.
    #[test]
    fn strict_defaults_all_security_fields_pinned() {
        let cfg = RunScriptConfig::strict_defaults();
        assert_eq!(cfg.mode, ExecutionMode::Strict);
        assert!(cfg.isolate_home);
        // session_cwd None → resolve_cwd returns per-run tmpdir in Strict.
        assert!(cfg.session_cwd.is_none());
        // Non-allowlisted tools count check: strict allowlist ⊆ full allowlist.
        assert!(cfg.allowed_tools.len() <= RPC_ALLOWED_TOOLS.len());
        assert_eq!(
            cfg.allowed_tools.len(),
            RPC_ALLOWED_TOOLS.len() - UNSAFE_IN_STRICT.len(),
            "strict allowlist size = full - unsafe"
        );
    }

    // ── resolve_timeout (R14 + R3.5: clamp LLM, preserve caller) ─────────

    #[test]
    fn resolve_timeout_absent_uses_caller_config() {
        let r = resolve_timeout(None, Duration::from_secs(120));
        assert_eq!(r, Duration::from_secs(120));
    }

    #[test]
    fn resolve_timeout_llm_zero_clamps_to_minimum() {
        let r = resolve_timeout(Some(&serde_json::json!(0)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(MIN_TIMEOUT_SECS));
    }

    #[test]
    fn resolve_timeout_llm_huge_clamps_to_maximum() {
        let r = resolve_timeout(Some(&serde_json::json!(3600)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(MAX_TIMEOUT_SECS));
    }

    #[test]
    fn resolve_timeout_llm_negative_treated_as_absent() {
        // JSON -1 is not a valid u64 → treated as absent → caller wins.
        let r = resolve_timeout(Some(&serde_json::json!(-1)), Duration::from_secs(120));
        assert_eq!(r, Duration::from_secs(120));
    }

    #[test]
    fn resolve_timeout_llm_string_treated_as_absent() {
        let r = resolve_timeout(Some(&serde_json::json!("five")), Duration::from_secs(120));
        assert_eq!(r, Duration::from_secs(120));
    }

    #[test]
    fn resolve_timeout_llm_valid_passes_clamped() {
        let r = resolve_timeout(Some(&serde_json::json!(60)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(60));
    }

    // R3.5: caller-supplied timeout larger than MAX_TIMEOUT_SECS must NOT
    // be silently clamped — only LLM-supplied values are clamped.
    #[test]
    fn resolve_timeout_preserves_caller_config_beyond_max() {
        let big = Duration::from_secs(MAX_TIMEOUT_SECS * 2);
        let r = resolve_timeout(None, big);
        assert_eq!(r, big, "caller-supplied timeout must be trusted verbatim");
    }

    // R3.5: caller config below MIN is also preserved (test env often uses
    // sub-second timeouts intentionally).
    #[test]
    fn resolve_timeout_preserves_caller_config_below_min() {
        let small = Duration::from_millis(500);
        let r = resolve_timeout(None, small);
        assert_eq!(r, small);
    }

    // T30: float timeouts are accepted (LLMs occasionally emit 3.0 for integers).
    #[test]
    fn resolve_timeout_float_accepted_as_integer() {
        let r = resolve_timeout(Some(&serde_json::json!(3.0)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(3));
    }

    #[test]
    fn resolve_timeout_float_truncates_toward_zero() {
        let r = resolve_timeout(Some(&serde_json::json!(3.9)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(3));
    }

    // T43: 0.4 truncates to 0, then clamps up to MIN_TIMEOUT_SECS.
    // Confirms fractional-small-positive floats don't produce zero timeouts.
    #[test]
    fn resolve_timeout_small_positive_float_clamps_to_minimum() {
        let r = resolve_timeout(Some(&serde_json::json!(0.4)), Duration::from_secs(300));
        assert_eq!(r, Duration::from_secs(MIN_TIMEOUT_SECS));
    }

    #[test]
    fn resolve_timeout_float_negative_treated_as_absent() {
        let r = resolve_timeout(Some(&serde_json::json!(-0.5)), Duration::from_secs(120));
        assert_eq!(r, Duration::from_secs(120));
    }

    // R5.5: serde_json refuses to construct a JSON Number from NaN/Inf
    // (they aren't valid JSON), so the is_finite check in
    // `parse_timeout_secs` cannot fire via the JSON path. We keep the
    // check as cheap defense for callers who construct Values directly,
    // and we pin the JSON-level guarantee here.
    #[test]
    fn resolve_timeout_json_cannot_carry_non_finite_floats() {
        // from_f64 is the only path to a float Number; it returns None for
        // non-finite inputs, so no Value::Number::Float can carry NaN/Inf.
        assert!(serde_json::Number::from_f64(f64::NAN).is_none());
        assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
        assert!(serde_json::Number::from_f64(f64::NEG_INFINITY).is_none());
    }

    // Strings that look like "NaN" don't deserialize to a number — they
    // go through the as_f64 None branch and we fall back to caller default.
    #[test]
    fn resolve_timeout_string_nan_treated_as_absent() {
        let r = resolve_timeout(Some(&serde_json::json!("NaN")), Duration::from_secs(120));
        assert_eq!(r, Duration::from_secs(120));
    }

    // T46: parse_timeout_secs rejects negatives directly — probes the
    // Rust-side guard independent of the JSON layer.
    #[test]
    fn parse_timeout_secs_rejects_negative_float_directly() {
        let v = serde_json::json!(-0.1);
        assert_eq!(parse_timeout_secs(&v), None);
    }

    #[test]
    fn parse_timeout_secs_accepts_zero_float() {
        // 0.0 truncates to 0 — a valid u64 — caller is expected to clamp.
        let v = serde_json::json!(0.0);
        assert_eq!(parse_timeout_secs(&v), Some(0));
    }

    #[test]
    fn parse_timeout_secs_rejects_null() {
        let v = serde_json::Value::Null;
        assert_eq!(parse_timeout_secs(&v), None);
    }

    #[test]
    fn parse_timeout_secs_rejects_object() {
        let v = serde_json::json!({"secs": 5});
        assert_eq!(parse_timeout_secs(&v), None);
    }

    // T58: JSON arrays are rejected (closes the "non-scalar" coverage).
    #[test]
    fn parse_timeout_secs_rejects_array() {
        let v = serde_json::json!([5]);
        assert_eq!(parse_timeout_secs(&v), None);
    }

    #[test]
    fn parse_timeout_secs_rejects_bool() {
        assert_eq!(parse_timeout_secs(&serde_json::json!(true)), None);
    }

    // ── PriorityHint rename ──────────────────────────────────────────────

    #[test]
    fn priority_hint_default_is_neutral() {
        assert_eq!(PriorityHint::default(), PriorityHint::Neutral);
    }

    // ── Error Display ────────────────────────────────────────────────────

    // T54 / R7.3: ScriptFailed's Display output is bounded regardless of
    // how big the underlying stdout/stderr buffers are. The full streams
    // stay in the struct fields for callers who need them.
    #[test]
    fn script_failed_display_is_bounded_with_huge_streams() {
        let err = RunScriptError::ScriptFailed {
            code: 1,
            stdout: "a".repeat(200_000),
            stderr: "b".repeat(200_000),
        };
        let display = err.to_string();
        // Budget: 2× preview cap + ~200 bytes of formatting scaffolding +
        // per-truncation notice header (the notice itself has some overhead).
        // 4 KB is a safe upper bound; anything under 100 KB proves bounded.
        assert!(
            display.len() < 4 * 1024,
            "Display output not bounded: {} bytes",
            display.len()
        );
        // Must still carry enough context to be useful.
        assert!(display.contains("code 1"));
        assert!(display.contains("stderr:"));
        assert!(display.contains("stdout:"));
    }

    // Full streams are preserved in the struct for programmatic access.
    #[test]
    fn script_failed_preserves_full_streams_in_fields() {
        let huge_stderr = "b".repeat(200_000);
        let err = RunScriptError::ScriptFailed {
            code: 1,
            stdout: "a".repeat(200_000),
            stderr: huge_stderr.clone(),
        };
        if let RunScriptError::ScriptFailed { stderr, .. } = err {
            assert_eq!(stderr.len(), huge_stderr.len());
        } else {
            unreachable!();
        }
    }

    // ── Schema generation ────────────────────────────────────────────────

    #[test]
    fn schema_lists_only_enabled_tools() {
        let enabled: HashSet<String> = ["read_file", "write_file"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("read_file"));
        assert!(desc.contains("write_file"));
        assert!(!desc.contains("web_fetch"));
        assert!(!desc.contains("bash"));
    }

    // T44: schema's timeout description tracks the timeout constants.
    // If MIN/MAX/DEFAULT change, the schema stays in sync automatically.
    #[test]
    fn schema_timeout_description_reflects_constants() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let timeout_desc = schema["function"]["parameters"]["properties"]["timeout"]["description"]
            .as_str()
            .unwrap();
        assert!(
            timeout_desc.contains(&MIN_TIMEOUT_SECS.to_string()),
            "timeout desc missing MIN={}: {}",
            MIN_TIMEOUT_SECS,
            timeout_desc
        );
        assert!(
            timeout_desc.contains(&MAX_TIMEOUT_SECS.to_string()),
            "timeout desc missing MAX={}: {}",
            MAX_TIMEOUT_SECS,
            timeout_desc
        );
        assert!(
            timeout_desc.contains(&DEFAULT_TIMEOUT.as_secs().to_string()),
            "timeout desc missing DEFAULT={}: {}",
            DEFAULT_TIMEOUT.as_secs(),
            timeout_desc
        );
    }

    // T41: extension tools (names NOT in TOOL_DOC_LINES) are silently
    // skipped rather than causing a panic or malformed schema. Keeps
    // run_script forward-compatible when callers extend allowed_tools
    // without updating the module's doc-line table.
    #[test]
    fn schema_skips_unknown_tool_names_gracefully() {
        let enabled: HashSet<String> = ["read_file", "nonstandard_extension_tool"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let desc = schema["function"]["description"].as_str().unwrap();
        // Known tool appears; unknown one is omitted (no panic, no raw name leak).
        assert!(desc.contains("read_file"));
        assert!(
            !desc.contains("nonstandard_extension_tool"),
            "unknown tool should not appear in schema desc; got: {desc}"
        );
    }

    // T34: when bash is disabled, neither the doc lines nor the import
    // example mention it. This guards against the model being misled into
    // trying `astra_tools.bash(...)` which would get rejected at the RPC
    // allowlist layer but produce a noisy error.
    #[test]
    fn schema_omits_bash_when_disabled() {
        let enabled: HashSet<String> = ["read_file", "grep"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let desc = schema["function"]["description"].as_str().unwrap();
        let script_desc = schema["function"]["parameters"]["properties"]["script"]["description"]
            .as_str()
            .unwrap();
        // No `bash(` call suggestion, no `bash,` import example token.
        assert!(
            !desc.contains("bash("),
            "desc leaks bash when disabled: {desc}"
        );
        assert!(
            !desc.contains(" bash,"),
            "desc lists bash in imports when disabled: {desc}"
        );
        assert!(
            !script_desc.contains("bash("),
            "script desc leaks bash: {script_desc}"
        );
    }

    #[test]
    fn schema_pinned_hint_present() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Pinned);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("PREFERRED"));
        assert!(desc.contains("pinned"));
    }

    #[test]
    fn schema_deprioritized_hint_present() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let schema = build_run_script_schema(
            &enabled,
            ExecutionMode::Project,
            PriorityHint::Deprioritized,
        );
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("DEPRIORITIZED"));
        // T48: Deprioritized is exclusive of Pinned — no PREFERRED marker leak.
        assert!(
            !desc.contains("PREFERRED"),
            "Deprioritized schema must not carry Pinned marker: {desc}"
        );
    }

    // T48 (reverse): Pinned is exclusive of Deprioritized.
    #[test]
    fn schema_pinned_hint_excludes_deprioritized_marker() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Pinned);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("PREFERRED"));
        assert!(
            !desc.contains("DEPRIORITIZED"),
            "Pinned schema must not carry Deprioritized marker: {desc}"
        );
    }

    #[test]
    fn schema_neutral_has_no_priority_hint() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(!desc.contains("PREFERRED"));
        assert!(!desc.contains("DEPRIORITIZED"));
    }

    // C1: empty enabled_tools must not produce malformed `from astra_tools import , ...`
    #[test]
    fn schema_empty_enabled_tools_graceful() {
        let enabled: HashSet<String> = HashSet::new();
        let schema =
            build_run_script_schema(&enabled, ExecutionMode::Project, PriorityHint::Neutral);
        let desc = schema["function"]["description"].as_str().unwrap();
        let script_desc = schema["function"]["parameters"]["properties"]["script"]["description"]
            .as_str()
            .unwrap();

        // ── Negative: no broken import hint leaks through.
        // Catches `from astra_tools import , ...` and its moral equivalents.
        let broken_patterns = ["import , ", "import , `", "import , ."];
        for bad in broken_patterns {
            assert!(
                !desc.contains(bad),
                "description contains broken import pattern {bad:?}: {desc}"
            );
            assert!(
                !script_desc.contains(bad),
                "script desc contains broken import pattern {bad:?}: {script_desc}"
            );
        }

        // ── Positive: empty state is explicitly communicated (two independent checks).
        assert!(
            desc.contains("stdlib"),
            "description must mention Python stdlib as the fallback: {desc}"
        );
        // Script param description must also guide the model toward stdlib-only use.
        assert!(
            script_desc.contains("stdlib"),
            "script param description must mention stdlib: {script_desc}"
        );
        // Defensive: no trailing import hint left dangling when there's no example to show.
        assert!(
            !script_desc.contains("astra_tools import ") || script_desc.contains("No tool"),
            "script desc should not advertise astra_tools import when no tools are enabled: {script_desc}"
        );
    }

    // ── Python stub generation ───────────────────────────────────────────

    #[test]
    fn stub_contains_only_enabled_tools() {
        let enabled: HashSet<String> = ["read_file", "grep"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let stub = generate_python_stub(&enabled);
        assert!(stub.contains("def read_file("));
        assert!(stub.contains("def grep("));
        assert!(!stub.contains("def write_file("));
        assert!(!stub.contains("def bash("));
    }

    #[test]
    fn stub_contains_builtin_helpers() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let stub = generate_python_stub(&enabled);
        assert!(stub.contains("def json_parse("));
        assert!(stub.contains("def shell_quote("));
        assert!(stub.contains("def retry("));
    }

    #[test]
    fn stub_has_try_finally_for_fd_safety() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let stub = generate_python_stub(&enabled);
        assert!(
            stub.contains("try:"),
            "stub must wrap socket in try/finally"
        );
        assert!(
            stub.contains("finally:"),
            "stub must close socket in finally"
        );
        assert!(stub.contains("sock.close()"));
    }

    #[test]
    fn stub_uses_per_call_connection() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let stub = generate_python_stub(&enabled);
        assert!(stub.contains("socket.AF_UNIX"));
        assert!(stub.contains("sock.connect("));
        assert!(stub.contains("sock.shutdown("));
    }

    // R4.8: tool name in `_call("name", ...)` is JSON-encoded (double-quoted)
    // so non-ASCII / backslash / quote-containing names never break Python
    // parsing. All current tool names are ASCII, so we assert the expected
    // literal form.
    #[test]
    fn stub_encodes_tool_name_as_double_quoted_literal() {
        let enabled: HashSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let stub = generate_python_stub(&enabled);
        // JSON encoding gives `"read_file"` (double-quoted). Rust's `{:?}`
        // debug would also give double quotes here, but JSON guarantees
        // cross-language literal safety even for hypothetical odd names.
        assert!(
            stub.contains(r#"return _call("read_file", {"#),
            "stub should use JSON-encoded literal for tool name; got:\n{stub}"
        );
    }

    // ── Execution-mode resolvers ─────────────────────────────────────────

    #[test]
    fn resolve_python_strict_always_python3() {
        assert_eq!(resolve_python(ExecutionMode::Strict), "python3");
    }

    #[test]
    fn resolve_cwd_strict_uses_fallback() {
        let fb = PathBuf::from("/tmp/fallback");
        let r = resolve_cwd(ExecutionMode::Strict, Some(Path::new("/tmp")), &fb);
        assert_eq!(r, fb);
    }

    // T47: session_cwd is completely ignored in Strict mode — even when
    // it points at an existing directory. Strict mode's contract is
    // "per-run tmpdir, no session bleed". Pin the contract.
    #[test]
    fn resolve_cwd_strict_ignores_even_valid_session_cwd() {
        let fb = PathBuf::from("/tmp");
        // "/tmp" is a real dir, so Project mode would use it. Strict
        // must still return fallback.
        let r = resolve_cwd(ExecutionMode::Strict, Some(Path::new("/tmp")), &fb);
        assert_eq!(r, fb, "Strict must not honor session_cwd");
    }

    #[test]
    fn resolve_cwd_project_uses_session_cwd_if_exists() {
        let session = PathBuf::from("/tmp");
        let fb = PathBuf::from("/tmp/fallback");
        let r = resolve_cwd(ExecutionMode::Project, Some(&session), &fb);
        assert_eq!(r, session);
    }

    // R4.3: Project mode with a bad session_cwd falls to `fallback`
    // (per-run tmpdir) — NOT to std::env::current_dir(). Using the
    // process CWD would leak coincidental ambient state into each run.
    #[test]
    fn resolve_cwd_project_falls_back_to_fallback_on_missing() {
        let fb = PathBuf::from("/tmp");
        let bad = Path::new("/nonexistent/path/unlikely/definitely");
        let r = resolve_cwd(ExecutionMode::Project, Some(bad), &fb);
        assert_eq!(r, fb, "must use per-run fallback, not process CWD");
    }

    // T28: Project mode with session_cwd=None uses fallback directly.
    #[test]
    fn resolve_cwd_project_none_session_uses_fallback() {
        let fb = PathBuf::from("/tmp");
        let r = resolve_cwd(ExecutionMode::Project, None, &fb);
        assert_eq!(r, fb);
    }

    // T55: an empty PathBuf is not a directory (`is_dir()` returns false),
    // so `resolve_cwd` correctly falls through to the fallback rather than
    // accepting a malformed session_cwd.
    #[test]
    fn resolve_cwd_project_empty_session_cwd_uses_fallback() {
        let fb = PathBuf::from("/tmp");
        let empty = PathBuf::new();
        let r = resolve_cwd(ExecutionMode::Project, Some(&empty), &fb);
        assert_eq!(r, fb, "empty PathBuf session_cwd must be rejected");
    }

    // C7: nonexistent python path doesn't panic
    #[test]
    fn is_usable_python_nonexistent_returns_false() {
        clear_python_cache();
        assert!(!is_usable_python("/nonexistent/path/to/python"));
    }

    // T53: PythonFingerprint::for_path on a nonexistent path returns
    // all-None. Two such fingerprints compare equal (dedup correctness).
    #[test]
    fn python_fingerprint_nonexistent_path_returns_none_fields() {
        let fp = PythonFingerprint::for_path("/nonexistent/path/no-such-file");
        assert!(fp.mtime.is_none());
        assert!(fp.len.is_none());
        // Two probes of the same nonexistent path must match.
        let fp2 = PythonFingerprint::for_path("/nonexistent/path/no-such-file");
        assert_eq!(fp, fp2, "unstat-able paths must fingerprint-match");
    }

    // T59: is_usable_python on a non-executable file returns false cleanly.
    // ExecvpOp returns EACCES/permission-denied; we swallow it.
    #[serial]
    #[test]
    fn is_usable_python_non_executable_file_returns_false() {
        clear_python_cache();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a binary").unwrap();
        // Ensure it's NOT executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(tmp.path()).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(tmp.path(), perms).unwrap();
        }
        let path = tmp.path().to_str().unwrap();
        assert!(
            !is_usable_python(path),
            "non-executable file must probe false"
        );
    }

    // R6: repeated is_usable_python calls hit the cache (same result +
    // cache size stays at 1 entry for the unique path).
    //
    // We can't use the global probe counter reliably because other tests
    // running in parallel (not `#[serial]`) may probe `python3` through
    // `python3_available()` and increment it. Instead we inspect the
    // cache itself, which only this `#[serial]` test writes to for this key.
    #[serial]
    #[test]
    fn is_usable_python_caches_result() {
        clear_python_cache();
        let r1 = is_usable_python("/nonexistent/path/cache-test");
        let r2 = is_usable_python("/nonexistent/path/cache-test");
        assert_eq!(r1, r2);
        // Exactly one entry was written for this path.
        let hits = python_cache()
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _, _)| k == "/nonexistent/path/cache-test")
            .count();
        assert_eq!(hits, 1, "second call should hit cache, not re-insert");
    }

    // R6.1 / R7.2: when a Python binary's fingerprint changes, the cache
    // entry is invalidated. We prove this by mutating either the file's
    // size (R7.1 addition) OR its mtime — both fingerprint components
    // must cause re-probing.
    //
    // The file isn't a real interpreter, so is_usable_python returns
    // false both times; the test's claim is strictly about fingerprint
    // dedup, which is the cache-correctness contract.
    #[serial]
    #[test]
    fn python_cache_invalidates_on_fingerprint_change() {
        clear_python_cache();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // Round 1: probe, cache now holds (path, fp1).
        std::fs::write(tmp.path(), b"first").unwrap();
        let _ = is_usable_python(&path);
        let fp_first = python_cache()
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _, _)| k == &path)
            .map(|(_, fp, _)| *fp)
            .expect("entry inserted on first probe");

        // Change content AND size (belt + suspenders — any single one
        // would be sufficient; both drive home that either triggers
        // invalidation). No sleep needed since size bumps alone are
        // detected even within the same second.
        std::fs::write(tmp.path(), b"second-content-that-is-longer").unwrap();

        let _ = is_usable_python(&path);
        let fp_second = python_cache()
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _, _)| k == &path)
            .map(|(_, fp, _)| *fp)
            .expect("entry re-inserted on second probe");

        assert_ne!(
            fp_first, fp_second,
            "fingerprint must refresh on content mutation"
        );
        assert_ne!(fp_first.len, fp_second.len, "size component must differ");

        // Dedup contract: exactly one entry for this path.
        let count = python_cache()
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _, _)| k == &path)
            .count();
        assert_eq!(count, 1, "stale entries should not accumulate");
    }

    // R7.1: size alone (without mtime bump) is enough to invalidate.
    // This is the critical case the old mtime-only fingerprint missed —
    // rebuilds completing within the same mtime-second still change size.
    #[serial]
    #[test]
    fn python_cache_size_only_change_invalidates() {
        clear_python_cache();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        std::fs::write(tmp.path(), b"a").unwrap();
        let fp1 = PythonFingerprint::for_path(&path);

        std::fs::write(tmp.path(), b"aa").unwrap();
        let fp2 = PythonFingerprint::for_path(&path);

        assert_ne!(fp1, fp2, "different size → different fingerprint");
        // Even if mtime happens to match (same-second rewrite), size differs
        // and the fingerprints are not equal.
        if fp1.mtime == fp2.mtime {
            assert_ne!(fp1.len, fp2.len, "fallback: size must still differ");
        }
    }

    // R5.6 / T45: cache is bounded to PYTHON_CACHE_CAP entries.
    #[serial]
    #[test]
    fn python_cache_is_bounded() {
        clear_python_cache();
        // Stuff more than the cap with distinct non-existent paths. Each
        // probe fails fast (std::process::Command on a bogus path), so
        // this test is cheap.
        for i in 0..(PYTHON_CACHE_CAP + 10) {
            let _ = is_usable_python(&format!("/nonexistent/probe-{i}"));
        }
        let size = python_cache().lock().unwrap().len();
        assert!(
            size <= PYTHON_CACHE_CAP,
            "cache should never exceed cap={PYTHON_CACHE_CAP}, got {size}"
        );
    }

    // T51: after eviction, the most recently inserted entries are retained
    // (not the oldest). Critical for cache effectiveness — if we evicted
    // newer entries, the cache would thrash under sustained load.
    #[serial]
    #[test]
    fn python_cache_retains_most_recent_after_eviction() {
        clear_python_cache();
        // Insert CAP + 10 entries. After FIFO-half eviction, the most
        // recent 10+ entries must still be present; the oldest half are gone.
        let total = PYTHON_CACHE_CAP + 10;
        for i in 0..total {
            let _ = is_usable_python(&format!("/nonexistent/recent-{i}"));
        }
        let cache = python_cache().lock().unwrap();
        // Last inserted path MUST be present.
        let last_path = format!("/nonexistent/recent-{}", total - 1);
        assert!(
            cache.iter().any(|(k, _, _)| k == &last_path),
            "most recent entry dropped by eviction: {last_path}"
        );
        // First inserted path should have been evicted (fell in the
        // oldest-half drain).
        let first_path = "/nonexistent/recent-0".to_string();
        assert!(
            !cache.iter().any(|(k, _, _)| k == &first_path),
            "oldest entry should have been evicted: {first_path}"
        );
    }

    // ── build_child_env ──────────────────────────────────────────────────

    /// Build env bundle along with the tmpdir it references, so tests can
    /// assert HOME/PYTHONPATH equal the tmpdir path.
    fn test_env_bundle_with_tmp(
        mode: ExecutionMode,
        isolate_home: bool,
    ) -> (tempfile::TempDir, Vec<(String, String)>) {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("rpc.sock");
        let token = AuthToken::from_str_for_test("testtoken");
        let env = build_child_env(tmp.path(), &sock, &token, mode, isolate_home);
        (tmp, env)
    }

    fn test_env_bundle(mode: ExecutionMode, isolate_home: bool) -> Vec<(String, String)> {
        test_env_bundle_with_tmp(mode, isolate_home).1
    }

    fn env_get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn build_child_env_default_isolates_home() {
        let (tmp, env) = test_env_bundle_with_tmp(ExecutionMode::Strict, true);
        let home = env_get(&env, "HOME").unwrap();
        // HOME is exactly the per-run tmpdir — strict isolation.
        assert_eq!(home, tmp.path().display().to_string().as_str());
    }

    #[test]
    fn build_child_env_includes_rpc_pair_despite_auth_substring() {
        // ASTRA_RPC_AUTH_TOKEN contains AUTH/TOKEN — must not be filtered.
        let env = test_env_bundle(ExecutionMode::Strict, true);
        assert!(env_get(&env, "ASTRA_RPC_SOCKET").is_some());
        assert!(env_get(&env, "ASTRA_RPC_AUTH_TOKEN").is_some());
    }

    #[serial]
    #[test]
    fn build_child_env_opt_out_keeps_real_home() {
        // Set a known HOME so the assertion is deterministic.
        let _guard = EnvGuard::set("HOME", "/tmp/fake-home-for-test");
        let env = test_env_bundle(ExecutionMode::Strict, false);
        let home = env_get(&env, "HOME").unwrap();
        assert_eq!(home, "/tmp/fake-home-for-test");
    }

    // T38 / R5.4: HOME set to empty string must be treated as unset —
    // otherwise the child sees HOME="" which breaks os.path.expanduser.
    #[serial]
    #[test]
    fn build_child_env_empty_home_falls_back_to_tmpdir() {
        let _guard = EnvGuard::set("HOME", "");
        let (tmp, env) = test_env_bundle_with_tmp(ExecutionMode::Strict, false);
        let home = env_get(&env, "HOME").unwrap();
        assert!(!home.is_empty(), "empty HOME must not propagate to child");
        assert_eq!(
            home,
            tmp.path().display().to_string().as_str(),
            "empty HOME should fall back to per-run tmpdir"
        );
    }

    // T29 / R4.1: isolate_home=false AND parent HOME unset must still
    // produce a safe fallback — the per-run tmpdir, not a hardcoded "/tmp"
    // that would misrepresent the process's real HOME.
    #[serial]
    #[test]
    fn build_child_env_home_unset_falls_back_to_tmpdir() {
        // Remove HOME for the duration of this test.
        let prior_home = std::env::var("HOME").ok();
        // SAFETY: test code, serialized via #[serial].
        unsafe { std::env::remove_var("HOME") };

        let (tmp, env) = test_env_bundle_with_tmp(ExecutionMode::Strict, false);
        let home = env_get(&env, "HOME").unwrap();
        assert_eq!(
            home,
            tmp.path().display().to_string().as_str(),
            "HOME must fall back to per-run tmpdir, not /tmp"
        );
        assert_ne!(home, "/tmp", "never fall back to hardcoded /tmp");

        // Restore HOME.
        // SAFETY: test code, serialized.
        if let Some(v) = prior_home {
            unsafe { std::env::set_var("HOME", v) };
        }
    }

    // S2: Strict mode emits a minimal PATH; Project mode inherits parent's.
    #[serial]
    #[test]
    fn build_child_env_strict_mode_uses_minimal_path() {
        let _guard = EnvGuard::set("PATH", "/opt/suspicious:/usr/bin:/bin");
        let env = test_env_bundle(ExecutionMode::Strict, true);
        let path = env_get(&env, "PATH").unwrap();
        assert_eq!(
            path, STRICT_PATH,
            "Strict mode must not inherit parent PATH"
        );
        assert!(!path.contains("/opt/suspicious"));
    }

    #[serial]
    #[test]
    fn build_child_env_project_mode_inherits_parent_path() {
        let _guard = EnvGuard::set("PATH", "/opt/project-tools:/usr/bin:/bin");
        let env = test_env_bundle(ExecutionMode::Project, true);
        let path = env_get(&env, "PATH").unwrap();
        assert!(
            path.contains("/opt/project-tools"),
            "Project mode must inherit parent PATH for venv/poetry/uv; got {path}"
        );
    }

    // T50: PATH order is preserved verbatim. Resolution priority matters —
    // if PATH were ever re-sorted/deduped silently, venv python might be
    // shadowed by a system binary.
    #[serial]
    #[test]
    fn build_child_env_project_mode_preserves_path_order() {
        let parent = "/opt/a:/opt/b:/opt/c:/usr/bin:/bin";
        let _guard = EnvGuard::set("PATH", parent);
        let env = test_env_bundle(ExecutionMode::Project, true);
        let path = env_get(&env, "PATH").unwrap();
        assert_eq!(path, parent, "PATH order must be preserved verbatim");
    }

    // ── handle_run_script arg parsing ────────────────────────────────────

    #[tokio::test]
    async fn handle_run_script_missing_script_param() {
        let exec = MockToolExecutor::new();
        let result =
            handle_run_script(&serde_json::json!({}), &exec, RunScriptConfig::default()).await;
        assert!(result.is_error);
        assert!(result.output.contains("Missing 'script'"));
    }

    // ── Integration tests (require Python) ───────────────────────────────

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_happy_path_multiple_rpc_calls() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            allowed_tools: ["read_file", "grep"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        let script = r#"
import astra_tools
r1 = astra_tools.read_file("a.txt")
r2 = astra_tools.grep("pattern")
print(f"{r1}|{r2}")
"#;
        let result = run_script(script, &config, &exec).await.unwrap();
        assert!(result.contains("content of a.txt"));
        assert!(result.contains("match: pattern"));
        assert_eq!(exec.call_count(), 2);
    }

    // C9: stdout before raise must be preserved in the error report.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_partial_stdout_preserved_on_error() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let script = r#"
print("partial_before_raise")
raise ValueError("boom")
"#;
        let err = run_script(script, &config, &exec).await.unwrap_err();
        match err {
            RunScriptError::ScriptFailed {
                code,
                stdout,
                stderr,
            } => {
                assert_ne!(code, 0);
                // Partial stdout lives in its OWN field — cleanly separable.
                assert!(
                    stdout.contains("partial_before_raise"),
                    "lost partial stdout: stdout={stdout:?} stderr={stderr:?}"
                );
                // stderr is strictly the traceback — no stdout mashed in.
                assert!(stderr.contains("ValueError"));
                assert!(stderr.contains("boom"));
                assert!(
                    !stderr.contains("partial_before_raise"),
                    "stderr should not contain stdout content: {stderr}"
                );
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    // T36: two run_script invocations run concurrently and each gets its
    // own per-run tmpdir / socket / auth token — no cross-talk.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_scripts_run_concurrently_without_cross_talk() {
        if !python3_available() {
            return;
        }

        async fn run_one(label: &'static str) -> String {
            let exec = MockToolExecutor::new();
            let config = RunScriptConfig {
                timeout: Duration::from_secs(10),
                mode: ExecutionMode::Strict,
                ..Default::default()
            };
            let script = format!("print({label:?})");
            run_script(&script, &config, &exec).await.unwrap()
        }

        let (a, b) = tokio::join!(run_one("alpha-marker"), run_one("bravo-marker"));
        assert!(
            a.contains("alpha-marker") && !a.contains("bravo"),
            "A tainted: {a}"
        );
        assert!(
            b.contains("bravo-marker") && !b.contains("alpha"),
            "B tainted: {b}"
        );
    }

    // R5.8 / T31 (inner layer): the raw API returns an empty string for
    // an empty script. Pinning this here prevents a future refactor from
    // moving the empty-output handling into run_script itself (which would
    // conflate the "no output" contract with the "noop" contract).
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_run_script_empty_source_returns_empty_ok() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(5),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let out = run_script("", &config, &exec).await.unwrap();
        assert_eq!(out, "", "run_script should return empty stdout verbatim");
    }

    // R5.8 / T31 (wrapper): handle_run_script substitutes the explicit
    // completion notice so the LLM doesn't receive a blank result it
    // might mis-interpret.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn handle_run_script_empty_source_returns_notice() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(5),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let result = handle_run_script(&serde_json::json!({"script": ""}), &exec, config).await;
        assert!(
            !result.is_error,
            "empty script must exit cleanly: {}",
            result.output
        );
        assert!(
            result.output.contains("completed with no output"),
            "expected empty-output notice, got: {}",
            result.output
        );
    }

    // T32: script that prints only to stderr and exits nonzero — stdout
    // field in ScriptFailed must be empty, stderr carries everything.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_stderr_only_nonzero_exit() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let script = r#"
import sys
sys.stderr.write("only-on-stderr\n")
sys.exit(7)
"#;
        match run_script(script, &config, &exec).await {
            Err(RunScriptError::ScriptFailed {
                code,
                stdout,
                stderr,
            }) => {
                assert_eq!(code, 7);
                assert!(stdout.is_empty(), "stdout should be empty, got: {stdout:?}");
                assert!(
                    stderr.contains("only-on-stderr"),
                    "stderr missing content: {stderr}"
                );
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    // T37: when the script exits nonzero, stdout captured in ScriptFailed
    // is still capped by max_stdout_bytes (head+tail truncation).
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_failed_exit_still_caps_stdout() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 200,
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let script = r#"
for i in range(100):
    print(f"line_{i:03d}_" + "x" * 20)
raise RuntimeError("after noise")
"#;
        match run_script(script, &config, &exec).await {
            Err(RunScriptError::ScriptFailed { stdout, stderr, .. }) => {
                // stdout got truncated — notice present + first line retained.
                assert!(
                    stdout.contains("OUTPUT TRUNCATED"),
                    "stdout cap not applied: {stdout}"
                );
                assert!(stdout.contains("line_000"), "head lost: {stdout}");
                assert!(stderr.contains("RuntimeError"), "stderr missing: {stderr}");
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    // R11: stderr truncation notice when flood exceeds cap.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_stderr_truncation_notice_present() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        // Print 50KB to stderr — way over the 10KB cap — then exit nonzero.
        let script = r#"
import sys
sys.stderr.write("x" * 50000)
sys.stderr.flush()
sys.exit(2)
"#;
        let err = run_script(script, &config, &exec).await.unwrap_err();
        match err {
            RunScriptError::ScriptFailed { stderr, .. } => {
                // stderr notice carries the [stderr] tag to distinguish
                // from the stdout-side truncation marker.
                assert!(
                    stderr.contains("[stderr OUTPUT TRUNCATED"),
                    "stderr truncation notice missing or untagged: got {} chars",
                    stderr.len()
                );
                // Paragraph break ensures the notice is readable even when
                // stderr doesn't end with a newline.
                assert!(
                    stderr.contains("\n\n... [stderr OUTPUT TRUNCATED"),
                    "notice should have a blank-line separator"
                );
            }
            other => panic!("expected ScriptFailed, got {other:?}"),
        }
    }

    // T24: child is actually killed when max_tool_calls is exceeded — not
    // merely left to run until the configured timeout expires.
    //
    // Budget: the script sleeps 1s between calls and would total ~20s if
    // unkilled. Configured timeout is 60s so the kill path is strictly the
    // only thing that can end this test in < 10s. Upper bound 15s gives
    // CI headroom for process-spawn latency spikes.
    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_exceeded_call_limit_kills_child_fast() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(60),
            max_tool_calls: 2,
            allowed_tools: ["read_file"].iter().map(|s| s.to_string()).collect(),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let script = r#"
import astra_tools, time
for i in range(20):
    try:
        astra_tools.read_file(f"file_{i}.txt")
    except RuntimeError:
        pass
    time.sleep(1)
print("LEAK: should not have reached here")
"#;
        let start = std::time::Instant::now();
        let _ = run_script(script, &config, &exec).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(15),
            "child was not killed — took {elapsed:?}, max_tool_calls kill path broken"
        );
        assert!(
            exec.call_count() <= config.max_tool_calls + 1,
            "executor saw {} calls, expected ≤ {}",
            exec.call_count(),
            config.max_tool_calls + 1,
        );
    }

    // Env hardening: secret env in parent never reaches child.
    #[tokio::test]
    #[serial]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_secret_env_not_visible_to_child() {
        if !python3_available() {
            return;
        }
        let _guard = EnvGuard::set("MY_SUPER_SECRET", "should-not-leak");
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            ..Default::default()
        };
        let script = r#"
import os
print(os.environ.get("MY_SUPER_SECRET", "UNSET"))
"#;
        let result = run_script(script, &config, &exec).await.unwrap();
        assert!(
            result.contains("UNSET"),
            "secret env var leaked to child: {result}"
        );
    }

    // HOME isolation: by default script's HOME is the per-run tmpdir.
    // Set HOME to a known sentinel so the assertion is robust on CI
    // runners that may or may not have HOME set to /root, /home/runner, etc.
    #[tokio::test]
    #[serial]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn live_script_home_is_isolated_by_default() {
        if !python3_available() {
            return;
        }
        let _guard = EnvGuard::set("HOME", "/tmp/sentinel-parent-home");
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            isolate_home: true,
            ..Default::default()
        };
        let script = r#"
import os
print(os.environ["HOME"])
"#;
        let result = run_script(script, &config, &exec).await.unwrap();
        // Must NOT be the parent sentinel — isolation puts HOME in the
        // per-run tmpdir created by tempfile, whose path starts with /tmp/.
        assert!(
            !result.contains("/tmp/sentinel-parent-home"),
            "HOME leaked parent value: {result}"
        );
    }

    // B4: cgroup v2 memory limit actually kills runaway scripts.
    //
    // Requires: Linux with cgroup v2 mounted, write access to
    // `/sys/fs/cgroup` (typically root or a systemd-delegated subtree).
    // CI without this setup is fine — the test is `#[ignore]`'d unless
    // the `cgroup_tests` feature is on.
    //
    // The script allocates a 200 MiB bytearray. Under a 64 MiB cgroup
    // memory ceiling, the allocation triggers the kernel OOM killer
    // (cgroup-level, not system-level) and the child dies with SIGKILL.
    // run_script surfaces that as ScriptFailed with no stderr (the kill
    // is sudden — Python doesn't get a chance to print a traceback).
    #[tokio::test]
    #[cfg_attr(not(feature = "cgroup_tests"), ignore)]
    async fn live_script_cgroup_memory_limit_kills_runaway() {
        if !python3_available() {
            return;
        }
        let exec = MockToolExecutor::new();
        let config = RunScriptConfig {
            timeout: Duration::from_secs(10),
            mode: ExecutionMode::Strict,
            memory_limit_bytes: 64 * 1024 * 1024, // 64 MiB ceiling
            cpu_quota: 1.0,
            ..Default::default()
        };
        // Try to allocate 200 MiB — cgroup should kill us before we finish.
        let script = r#"
data = bytearray(200 * 1024 * 1024)
print(f"allocated {len(data)} bytes — cgroup did not enforce limit")
"#;
        let result = run_script(script, &config, &exec).await;
        match result {
            Ok(out) => {
                panic!("script completed despite memory cap; cgroup not enforced: {out}");
            }
            Err(RunScriptError::ScriptFailed { code, .. }) => {
                // SIGKILL → exit code 137 (128 + 9) on typical shells,
                // or -9 / None depending on wait semantics. We just
                // check it's nonzero — the exact code varies.
                assert_ne!(code, 0, "cgroup-killed child must exit nonzero");
            }
            Err(other) => {
                panic!("expected ScriptFailed from cgroup OOM-kill, got: {other:?}");
            }
        }
    }
}
