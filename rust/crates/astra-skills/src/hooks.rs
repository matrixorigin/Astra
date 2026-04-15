//! Skill lifecycle hooks, tool event hooks, and session event hooks.
//!
//! Three hook systems coexist:
//!
//! 1. **Skill lifecycle hooks** (`SkillHooks`) — pre/post invocation of a skill itself.
//! 2. **Tool event hooks** (`ToolEventHook`) — fire on any tool call matching a pattern,
//!    inspired by Claude Code's PreToolUse / PostToolUse system.
//! 3. **Session event hooks** (`SessionEventHook`) — fire on session lifecycle events
//!    (SessionStart, SessionEnd, UserPromptSubmit, SubagentStart), compatible with
//!    Claude Code's hook event model.

use serde::{Deserialize, Serialize};

// ── Skill lifecycle hooks (existing) ─────────────────────────────────────

/// An action to execute as part of a hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookAction {
    /// Run a shell command.
    Shell { command: String },
    /// Set an environment variable.
    SetEnv { key: String, value: String },
    /// Custom hook identifier (for extensibility).
    Custom {
        id: String,
        config: Option<serde_json::Value>,
    },
    /// Send an HTTP webhook (POST with JSON body).
    Http {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        /// Timeout in seconds for the HTTP request (default: 10).
        #[serde(default = "default_hook_timeout")]
        timeout_secs: u32,
    },
}

/// Lifecycle hooks for a skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillHooks {
    /// Actions to run before skill invocation.
    #[serde(default)]
    pub pre_invoke: Vec<HookAction>,
    /// Actions to run after successful skill completion.
    #[serde(default)]
    pub post_invoke: Vec<HookAction>,
    /// Actions to run when skill execution fails.
    #[serde(default)]
    pub on_error: Vec<HookAction>,
}

impl SkillHooks {
    pub fn is_empty(&self) -> bool {
        self.pre_invoke.is_empty() && self.post_invoke.is_empty() && self.on_error.is_empty()
    }
}

// ── Tool event hooks (CC-inspired) ──────────────────────────────────────

/// When in the tool lifecycle the hook fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEventKind {
    /// Before the tool executes. Can block or inject context.
    PreToolUse,
    /// After the tool completes successfully. Can append context.
    PostToolUse,
}

/// Outcome of a pre-tool-use hook evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreToolDecision {
    /// Allow the tool call to proceed.
    Allow,
    /// Allow, but inject additional context into the tool output.
    AllowWithContext(String),
    /// Block the tool call with a reason.
    Block(String),
}

/// A single tool event hook configuration.
///
/// Configured in project settings (`.astra/hooks.json`) or skill frontmatter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEventHook {
    /// Which event triggers this hook.
    pub event: ToolEventKind,
    /// Tool name matcher — glob-style pattern (e.g. `"bash"`, `"read_*"`, `"*"`).
    /// If empty or `"*"`, matches all tools.
    #[serde(default)]
    pub matcher: String,
    /// The action to execute when the hook fires.
    pub action: HookAction,
    /// Optional timeout in seconds for shell actions (default: 10).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u32,
    /// Execute asynchronously (non-blocking). Default: false.
    #[serde(default)]
    pub is_async: bool,
    /// Execution priority — lower numbers run first. Default: 0.
    #[serde(default)]
    pub priority: i32,
    /// If true, execute only once per session then auto-disable. Default: false.
    #[serde(default)]
    pub once: bool,
    /// Optional condition expression (simple key=value checks on tool args).
    /// e.g. `"tool_name=bash"` or `"path=*.rs"`.
    #[serde(default)]
    pub condition: Option<String>,
}

fn default_hook_timeout() -> u32 {
    10
}

impl ToolEventHook {
    /// Check if this hook's matcher matches the given tool name.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        let pattern = self.matcher.trim();
        if pattern.is_empty() || pattern == "*" {
            return true;
        }
        glob_match(pattern, tool_name)
    }
}

/// Simple glob matching: `*` matches any sequence, `?` matches one char.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// A collection of tool event hooks with efficient lookup.
#[derive(Debug, Default)]
pub struct ToolEventHookRegistry {
    hooks: Vec<ToolEventHook>,
    /// Track which `once` hooks have already fired (by index in `hooks`).
    fired_once: std::sync::Mutex<std::collections::HashSet<usize>>,
}

impl ToolEventHookRegistry {
    pub fn new(hooks: Vec<ToolEventHook>) -> Self {
        Self {
            hooks,
            fired_once: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Return all hooks that match the given event kind and tool name,
    /// sorted by priority (ascending) and filtered by `once` status.
    pub fn matching(&self, event: ToolEventKind, tool_name: &str) -> Vec<&ToolEventHook> {
        let fired = self.fired_once.lock().unwrap_or_else(|e| e.into_inner());
        let mut result: Vec<(usize, &ToolEventHook)> = self
            .hooks
            .iter()
            .enumerate()
            .filter(|(i, h)| {
                h.event == event && h.matches_tool(tool_name) && !(h.once && fired.contains(i))
            })
            .collect();
        result.sort_by_key(|(_, h)| h.priority);
        result.into_iter().map(|(_, h)| h).collect()
    }

    /// Mark a `once` hook as fired (by matching identity).
    pub fn mark_once_fired(&self, hook: &ToolEventHook) {
        if !hook.once {
            return;
        }
        let mut fired = self.fired_once.lock().unwrap_or_else(|e| e.into_inner());
        for (i, h) in self.hooks.iter().enumerate() {
            if std::ptr::eq(h, hook) {
                fired.insert(i);
                return;
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }
}

// ── Config loading ──────────────────────────────────────────────────────────

/// File names to search for in `.astra/` directory.
const HOOK_CONFIG_CANDIDATES: &[&str] = &["hooks.json", "hooks.yaml", "hooks.yml"];

/// Load tool event hooks from the project's `.astra/` directory.
///
/// Searches for `hooks.json`, `hooks.yaml`, or `hooks.yml` under
/// `<project_root>/.astra/`. Returns an empty registry if no file is found
/// or if parsing fails (with a warning log).
pub fn load_tool_event_hooks(project_root: &std::path::Path) -> ToolEventHookRegistry {
    let (tool_hooks, _) = load_all_hooks(project_root);
    tool_hooks
}

/// Load session event hooks from the project's `.astra/` directory.
pub fn load_session_event_hooks(project_root: &std::path::Path) -> SessionEventHookRegistry {
    let (_, session_hooks) = load_all_hooks(project_root);
    session_hooks
}

/// Load both tool and session event hooks from a single config file.
pub fn load_all_hooks(
    project_root: &std::path::Path,
) -> (ToolEventHookRegistry, SessionEventHookRegistry) {
    let astra_dir = project_root.join(".astra");
    if !astra_dir.is_dir() {
        return (
            ToolEventHookRegistry::default(),
            SessionEventHookRegistry::default(),
        );
    }

    for candidate in HOOK_CONFIG_CANDIDATES {
        let path = astra_dir.join(candidate);
        if path.is_file() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let (tool_hooks, session_hooks) = if candidate.ends_with(".json") {
                        parse_all_hooks_json(&content, &path)
                    } else {
                        parse_all_hooks_yaml(&content, &path)
                    };
                    if !tool_hooks.is_empty() || !session_hooks.is_empty() {
                        tracing::info!(
                            target: "hook",
                            "Loaded {} tool + {} session hooks from {}",
                            tool_hooks.len(),
                            session_hooks.len(),
                            path.display()
                        );
                    }
                    return (
                        ToolEventHookRegistry::new(tool_hooks),
                        SessionEventHookRegistry::new(session_hooks),
                    );
                }
                Err(e) => {
                    tracing::warn!(target: "hook", "Failed to read {}: {}", path.display(), e);
                }
            }
        }
    }

    (
        ToolEventHookRegistry::default(),
        SessionEventHookRegistry::default(),
    )
}

/// JSON format: top-level array of ToolEventHook objects, or an object with a "hooks" key
/// and optional `default_timeout_secs`. Session hooks live under `"session_hooks"`.
fn parse_all_hooks_json(
    content: &str,
    path: &std::path::Path,
) -> (Vec<ToolEventHook>, Vec<SessionEventHook>) {
    // Try direct array first (legacy: tool hooks only)
    if let Ok(hooks) = serde_json::from_str::<Vec<ToolEventHook>>(content) {
        return (hooks, Vec::new());
    }

    // Try wrapper with both tool and session hooks
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        hooks: Vec<ToolEventHook>,
        #[serde(default)]
        session_hooks: Vec<SessionEventHook>,
        default_timeout_secs: Option<u32>,
    }
    if let Ok(w) = serde_json::from_str::<Wrapper>(content) {
        return (
            apply_default_timeout(w.hooks, w.default_timeout_secs),
            apply_default_timeout_session(w.session_hooks, w.default_timeout_secs),
        );
    }

    tracing::warn!(
        target: "hook",
        "Failed to parse {}: expected JSON array or {{\"hooks\": [...]}}",
        path.display()
    );
    (Vec::new(), Vec::new())
}

/// YAML format: same as JSON — top-level list or `hooks:` + `session_hooks:` keys.
fn parse_all_hooks_yaml(
    content: &str,
    path: &std::path::Path,
) -> (Vec<ToolEventHook>, Vec<SessionEventHook>) {
    // Try direct array (legacy: tool hooks only)
    if let Ok(hooks) = serde_yaml::from_str::<Vec<ToolEventHook>>(content) {
        return (hooks, Vec::new());
    }

    // Try wrapper
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        hooks: Vec<ToolEventHook>,
        #[serde(default)]
        session_hooks: Vec<SessionEventHook>,
        default_timeout_secs: Option<u32>,
    }
    if let Ok(w) = serde_yaml::from_str::<Wrapper>(content) {
        return (
            apply_default_timeout(w.hooks, w.default_timeout_secs),
            apply_default_timeout_session(w.session_hooks, w.default_timeout_secs),
        );
    }

    tracing::warn!(
        target: "hook",
        "Failed to parse {}: expected YAML list or `hooks:` mapping",
        path.display()
    );
    (Vec::new(), Vec::new())
}

/// Apply `default_timeout_secs` to hooks that still have the built-in default (10).
fn apply_default_timeout(
    mut hooks: Vec<ToolEventHook>,
    default: Option<u32>,
) -> Vec<ToolEventHook> {
    if let Some(dt) = default {
        let builtin = default_hook_timeout();
        for h in &mut hooks {
            if h.timeout_secs == builtin {
                h.timeout_secs = dt;
            }
        }
    }
    hooks
}

/// Apply `default_timeout_secs` to session hooks that still have the built-in default.
fn apply_default_timeout_session(
    mut hooks: Vec<SessionEventHook>,
    default: Option<u32>,
) -> Vec<SessionEventHook> {
    if let Some(dt) = default {
        let builtin = default_hook_timeout();
        for h in &mut hooks {
            if h.timeout_secs == builtin {
                h.timeout_secs = dt;
            }
        }
    }
    hooks
}

// ── Hook execution ──────────────────────────────────────────────────────────

/// Maximum bytes to read from a hook's stdout. Prevents a runaway hook from
/// consuming unbounded memory. 256 KiB is generous for JSON context output.
pub const HOOK_STDOUT_MAX_BYTES: usize = 256 * 1024;

/// Read up to [`HOOK_STDOUT_MAX_BYTES`] from an async reader.
pub(crate) async fn read_capped(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];
    loop {
        match reader.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => {
                let remaining = HOOK_STDOUT_MAX_BYTES - buf.len();
                buf.extend_from_slice(&tmp[..n.min(remaining)]);
                if buf.len() >= HOOK_STDOUT_MAX_BYTES {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf
}

/// Convenience: fire SessionEnd hooks, ignoring output.
pub async fn fire_session_end(registry: &SessionEventHookRegistry, session_id: &str) {
    let _ = evaluate_session_hooks(registry, SessionEvent::SessionEnd, session_id, None).await;
}

/// Execute all PreToolUse hooks matching a tool name.
///
/// Returns the aggregate decision:
/// - If any hook returns Block, the tool is blocked.
/// - If any hook returns AllowWithContext, the context is appended.
/// - Otherwise, Allow.
///
/// Shell hooks receive tool info via stdin JSON and produce a JSON decision:
/// ```json
/// {"decision": "allow"}
/// {"decision": "block", "reason": "dangerous command"}
/// {"decision": "allow", "context": "extra info to append"}
/// ```
pub async fn evaluate_pre_tool_hooks(
    registry: &ToolEventHookRegistry,
    tool_name: &str,
    tool_args: &serde_json::Value,
) -> PreToolDecision {
    let hooks = registry.matching(ToolEventKind::PreToolUse, tool_name);
    if hooks.is_empty() {
        return PreToolDecision::Allow;
    }

    let mut accumulated_context: Vec<String> = Vec::new();

    for hook in hooks {
        // Async hooks: fire-and-forget in background
        if hook.is_async {
            let action = hook.action.clone();
            let tn = tool_name.to_string();
            let ta = tool_args.clone();
            tokio::spawn(async move {
                run_hook_action_fire_and_forget(&action, &tn, &ta).await;
            });
            continue;
        }

        match &hook.action {
            HookAction::Shell { command } => {
                let decision =
                    run_shell_pre_hook(command, tool_name, tool_args, hook.timeout_secs).await;
                match decision {
                    PreToolDecision::Block(reason) => return PreToolDecision::Block(reason),
                    PreToolDecision::AllowWithContext(ctx) => accumulated_context.push(ctx),
                    PreToolDecision::Allow => {}
                }
            }
            HookAction::Http {
                url,
                headers,
                timeout_secs,
            } => {
                let decision =
                    run_http_pre_hook(url, headers, tool_name, tool_args, *timeout_secs).await;
                match decision {
                    PreToolDecision::Block(reason) => return PreToolDecision::Block(reason),
                    PreToolDecision::AllowWithContext(ctx) => accumulated_context.push(ctx),
                    PreToolDecision::Allow => {}
                }
            }
            HookAction::SetEnv { key, value } => {
                // Safety: only used in single-threaded test/CLI contexts
                unsafe { std::env::set_var(key, value) };
            }
            HookAction::Custom { id, .. } => {
                tracing::warn!(
                    target: "hook",
                    "Custom hook '{}' matched tool '{}' — not yet implemented",
                    id,
                    tool_name
                );
            }
        }
    }

    if accumulated_context.is_empty() {
        PreToolDecision::Allow
    } else {
        PreToolDecision::AllowWithContext(accumulated_context.join("\n"))
    }
}

/// Execute all PostToolUse hooks matching a tool name.
///
/// Returns modified output if any hook changed it, otherwise None.
pub async fn evaluate_post_tool_hooks(
    registry: &ToolEventHookRegistry,
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_output: &str,
) -> Option<String> {
    let hooks = registry.matching(ToolEventKind::PostToolUse, tool_name);
    if hooks.is_empty() {
        return None;
    }

    let mut current_output = tool_output.to_string();

    for hook in hooks {
        // Async hooks: fire-and-forget in background
        if hook.is_async {
            let action = hook.action.clone();
            let tn = tool_name.to_string();
            let ta = tool_args.clone();
            let out = current_output.clone();
            tokio::spawn(async move {
                run_hook_action_fire_and_forget(&action, &tn, &ta).await;
                let _ = out; // capture for potential future use
            });
            continue;
        }

        match &hook.action {
            HookAction::Shell { command } => {
                if let Some(modified) = run_shell_post_hook(
                    command,
                    tool_name,
                    tool_args,
                    &current_output,
                    hook.timeout_secs,
                )
                .await
                {
                    current_output = modified;
                }
            }
            HookAction::Http {
                url,
                headers,
                timeout_secs,
            } => {
                if let Some(modified) = run_http_post_hook(
                    url,
                    headers,
                    tool_name,
                    tool_args,
                    &current_output,
                    *timeout_secs,
                )
                .await
                {
                    current_output = modified;
                }
            }
            HookAction::Custom { id, .. } => {
                tracing::warn!(
                    target: "hook",
                    "PostToolUse custom hook '{}' for '{}' — not yet implemented",
                    id,
                    tool_name
                );
            }
            _ => {}
        }
    }

    if current_output != tool_output {
        Some(current_output)
    } else {
        None
    }
}

/// Run a shell command for a PreToolUse hook, with timeout.
async fn run_shell_pre_hook(
    command: &str,
    tool_name: &str,
    tool_args: &serde_json::Value,
    timeout_secs: u32,
) -> PreToolDecision {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let input = serde_json::json!({
        "hook_event": "pre_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
    });

    let mut child = match Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hook", "Failed to spawn hook '{}': {}", command, e);
            return PreToolDecision::Allow;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
        drop(stdin);
    }

    let mut stdout_handle = child.stdout.take();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);

    // Read stdout first (capped) to prevent pipe-full deadlock, then wait.
    let read_fut = async {
        let buf = match stdout_handle.as_mut() {
            Some(s) => read_capped(s).await,
            None => Vec::new(),
        };
        let status = child.wait().await;
        (buf, status)
    };

    let wait_result = tokio::time::timeout(timeout, read_fut).await;
    match wait_result {
        Ok((_, Ok(status))) if !status.success() => PreToolDecision::Block(format!(
            "Hook '{}' exited with status {}",
            command,
            status.code().unwrap_or(-1)
        )),
        Ok((buf, Ok(_))) => parse_pre_hook_output(&buf),
        Ok((_, Err(e))) => {
            tracing::warn!(target: "hook", "Hook I/O error for '{}': {}", command, e);
            PreToolDecision::Allow
        }
        Err(_) => {
            let _ = child.kill().await;
            PreToolDecision::Block(format!(
                "Hook '{}' timed out after {}s",
                command, timeout_secs
            ))
        }
    }
}

/// Run a shell command for a PostToolUse hook, with timeout.
async fn run_shell_post_hook(
    command: &str,
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_output: &str,
    timeout_secs: u32,
) -> Option<String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let input = serde_json::json!({
        "hook_event": "post_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
        "tool_output": tool_output,
    });

    let mut child = match Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hook", "Failed to spawn post-hook '{}': {}", command, e);
            return None;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
        drop(stdin);
    }

    let mut stdout_handle = child.stdout.take();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);

    // Read stdout first (capped) to prevent pipe-full deadlock, then wait.
    let read_fut = async {
        let buf = match stdout_handle.as_mut() {
            Some(s) => read_capped(s).await,
            None => Vec::new(),
        };
        let status = child.wait().await;
        (buf, status)
    };

    match tokio::time::timeout(timeout, read_fut).await {
        Ok((buf, Ok(status))) if status.success() => parse_post_hook_output(&buf),
        _ => {
            let _ = child.kill().await;
            None
        }
    }
}

/// Parse stdout from a PreToolUse shell hook into a decision.
fn parse_pre_hook_output(stdout: &[u8]) -> PreToolDecision {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return PreToolDecision::Allow;
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        match v.get("decision").and_then(|d| d.as_str()) {
            Some("block") => {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by hook")
                    .to_string();
                PreToolDecision::Block(reason)
            }
            Some("allow") => {
                if let Some(ctx) = v.get("context").and_then(|c| c.as_str()) {
                    PreToolDecision::AllowWithContext(ctx.to_string())
                } else {
                    PreToolDecision::Allow
                }
            }
            _ => PreToolDecision::Allow,
        }
    } else {
        PreToolDecision::Allow
    }
}

/// Parse stdout from a PostToolUse shell hook for output modification.
fn parse_post_hook_output(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        v.get("output").and_then(|o| o.as_str()).map(String::from)
    } else {
        None
    }
}

// ── HTTP hook execution ─────────────────────────────────────────────────────

/// Execute an HTTP webhook for PreToolUse, returning a decision.
async fn run_http_pre_hook(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    timeout_secs: u32,
) -> PreToolDecision {
    let payload = serde_json::json!({
        "hook_event": "pre_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
    });

    match http_post_json(url, headers, &payload, timeout_secs).await {
        Some(body) => parse_pre_hook_output(body.as_bytes()),
        None => PreToolDecision::Allow,
    }
}

/// Execute an HTTP webhook for PostToolUse, returning modified output if any.
async fn run_http_post_hook(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_output: &str,
    timeout_secs: u32,
) -> Option<String> {
    let payload = serde_json::json!({
        "hook_event": "post_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
        "tool_output": tool_output,
    });

    match http_post_json(url, headers, &payload, timeout_secs).await {
        Some(body) => parse_post_hook_output(body.as_bytes()),
        None => None,
    }
}

/// POST JSON to a URL with timeout. Returns response body on success, None on failure.
async fn http_post_json(
    url: &str,
    extra_headers: &std::collections::HashMap<String, String>,
    payload: &serde_json::Value,
    timeout_secs: u32,
) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let timeout = std::time::Duration::from_secs(timeout_secs as u64);
    let body = payload.to_string();

    // Simple HTTP POST using tokio TcpStream (no external HTTP client dependency).
    // Parse URL to extract host, port, path.
    let url_str = url.trim();
    let (host, port, path) = if let Some(rest) = url_str.strip_prefix("http://") {
        parse_http_url(rest, 80)
    } else if url_str.starts_with("https://") {
        // HTTPS not supported without TLS — log warning and skip.
        tracing::warn!(target: "hook", "HTTP hook: HTTPS not supported, skipping {}", url);
        return None;
    } else {
        parse_http_url(url_str, 80)
    };

    let connect_fut = async {
        let addr = format!("{}:{}", host, port);
        let mut stream = TcpStream::connect(&addr).await.ok()?;

        let request = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{}",
            path,
            host,
            body.len(),
            extra_headers
                .iter()
                .map(|(k, v)| format!("{}: {}\r\n", k, v))
                .collect::<String>(),
            body
        );
        stream.write_all(request.as_bytes()).await.ok()?;

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.ok()?;

        let text = String::from_utf8_lossy(&response);
        // Simple HTTP response body extraction: find blank line after headers.
        text.find("\r\n\r\n").map(|idx| text[idx + 4..].to_string())
    };

    match tokio::time::timeout(timeout, connect_fut).await {
        Ok(Some(body)) => Some(body),
        _ => {
            tracing::warn!(target: "hook", "HTTP hook to {} timed out or failed", url);
            None
        }
    }
}

/// Parse an HTTP URL (without scheme) into (host, port, path).
fn parse_http_url(url_without_scheme: &str, default_port: u16) -> (String, u16, String) {
    let (host_port, path) = if let Some(idx) = url_without_scheme.find('/') {
        (
            &url_without_scheme[..idx],
            url_without_scheme[idx..].to_string(),
        )
    } else {
        (url_without_scheme, "/".to_string())
    };

    let (host, port) = if let Some(idx) = host_port.rfind(':') {
        let port_str = &host_port[idx + 1..];
        if let Ok(p) = port_str.parse::<u16>() {
            (host_port[..idx].to_string(), p)
        } else {
            (host_port.to_string(), default_port)
        }
    } else {
        (host_port.to_string(), default_port)
    };

    (host, port, path)
}

// ── Fire-and-forget helper for async hooks ──────────────────────────────────

/// Execute a hook action without waiting for completion or checking results.
async fn run_hook_action_fire_and_forget(
    action: &HookAction,
    tool_name: &str,
    tool_args: &serde_json::Value,
) {
    match action {
        HookAction::Shell { command } => {
            let _ = run_shell_pre_hook(command, tool_name, tool_args, 30).await;
        }
        HookAction::Http {
            url,
            headers,
            timeout_secs,
        } => {
            let payload = serde_json::json!({
                "hook_event": "async_hook",
                "tool_name": tool_name,
                "tool_input": tool_args,
            });
            let _ = http_post_json(url, headers, &payload, *timeout_secs).await;
        }
        _ => {}
    }
}

/// Events emitted during the skill lifecycle.
///
/// Can be consumed by telemetry, logging, or debugging systems.
#[derive(Clone, Debug)]
pub enum SkillLifecycleEvent {
    /// A skill was discovered from a source.
    Discovered {
        name: String,
        source: super::manifest::SkillSourceKind,
    },
    /// A skill's instructions were fully loaded.
    Loaded { name: String },
    /// A conditional skill was activated by a path match.
    Activated { name: String, trigger: String },
    /// A skill invocation started.
    Invoked {
        name: String,
        context: super::manifest::ExecutionContext,
    },
    /// A skill invocation completed successfully.
    Completed {
        name: String,
        tokens_used: u32,
        turns: u32,
    },
    /// A skill invocation failed.
    Failed { name: String, error: String },
}

// ── Session event hooks (CC-compatible) ─────────────────────────────────

/// Session lifecycle events that can trigger hooks.
///
/// Compatible with Claude Code's hook event model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEvent {
    /// Fires once when a new session begins, before the first LLM turn.
    SessionStart,
    /// Fires when a session ends (explicit `/quit`, timeout, or graceful close).
    SessionEnd,
    /// Fires after the user submits a prompt, before tool selection / LLM call.
    UserPromptSubmit,
    /// Fires when a sub-agent (delegation) is spawned.
    SubagentStart,
    /// Fires when a sub-agent completes (either success or failure).
    SubagentStop,
    /// Fires before context compaction begins.
    PreCompact,
    /// Fires after context compaction completes.
    PostCompact,
    /// Fires when a file has been written or modified by a tool.
    FileChanged,
    /// Fires when the working directory changes.
    CwdChanged,
    /// Fires when a turn begins (before LLM call).
    TurnStart,
    /// Fires when a turn completes (after tool execution + post-tool policy).
    TurnEnd,
}

/// A single session event hook configuration.
///
/// Configured in `.astra/hooks.json` / `.astra/hooks.yaml` alongside tool event hooks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventHook {
    /// Which session event triggers this hook.
    pub event: SessionEvent,
    /// The action to execute when the event fires.
    pub action: HookAction,
    /// Timeout in seconds for shell actions (default: 10).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u32,
    /// Execute asynchronously (non-blocking). Default: false.
    #[serde(default)]
    pub is_async: bool,
    /// Execution priority — lower numbers run first. Default: 0.
    #[serde(default)]
    pub priority: i32,
    /// If true, execute only once per session then auto-disable. Default: false.
    #[serde(default)]
    pub once: bool,
    /// Optional condition expression.
    #[serde(default)]
    pub condition: Option<String>,
}

/// Output from a session event hook execution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionHookOutput {
    /// Context to inject into the conversation (e.g. greeting, env info).
    pub context: Option<String>,
    /// Environment variables to set.
    pub env_vars: Vec<(String, String)>,
}

/// Registry of session event hooks with lookup by event type.
#[derive(Debug, Default)]
pub struct SessionEventHookRegistry {
    hooks: Vec<SessionEventHook>,
    /// Track which `once` hooks have already fired (by index).
    fired_once: std::sync::Mutex<std::collections::HashSet<usize>>,
}

impl SessionEventHookRegistry {
    pub fn new(hooks: Vec<SessionEventHook>) -> Self {
        Self {
            hooks,
            fired_once: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Return all hooks matching the given event, sorted by priority and filtered by `once`.
    pub fn matching(&self, event: SessionEvent) -> Vec<&SessionEventHook> {
        let fired = self.fired_once.lock().unwrap_or_else(|e| e.into_inner());
        let mut result: Vec<(usize, &SessionEventHook)> = self
            .hooks
            .iter()
            .enumerate()
            .filter(|(i, h)| h.event == event && !(h.once && fired.contains(i)))
            .collect();
        result.sort_by_key(|(_, h)| h.priority);
        result.into_iter().map(|(_, h)| h).collect()
    }

    /// Mark a `once` hook as fired.
    pub fn mark_once_fired(&self, hook: &SessionEventHook) {
        if !hook.once {
            return;
        }
        let mut fired = self.fired_once.lock().unwrap_or_else(|e| e.into_inner());
        for (i, h) in self.hooks.iter().enumerate() {
            if std::ptr::eq(h, hook) {
                fired.insert(i);
                return;
            }
        }
    }

    /// Check if any hooks exist for the given event (no allocation).
    pub fn has_event(&self, event: SessionEvent) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }
}

/// Execute all hooks for a session event.
///
/// Shell hooks receive event info via stdin JSON:
/// ```json
/// {"hook_event": "session_start", "session_id": "...", "user_message": "hello"}
/// ```
///
/// And produce optional JSON output:
/// ```json
/// {"context": "Welcome back! Last session was about X."}
/// ```
pub async fn evaluate_session_hooks(
    registry: &SessionEventHookRegistry,
    event: SessionEvent,
    session_id: &str,
    user_message: Option<&str>,
) -> SessionHookOutput {
    let hooks = registry.matching(event);
    if hooks.is_empty() {
        return SessionHookOutput::default();
    }

    let mut output = SessionHookOutput::default();
    let mut contexts: Vec<String> = Vec::new();

    for hook in hooks {
        match &hook.action {
            HookAction::Shell { command } => {
                if let Some(result) = run_shell_session_hook(
                    command,
                    event,
                    session_id,
                    user_message,
                    hook.timeout_secs,
                )
                .await
                {
                    if let Some(ctx) = result.context {
                        contexts.push(ctx);
                    }
                    output.env_vars.extend(result.env_vars);
                }
            }
            HookAction::SetEnv { key, value } => {
                output.env_vars.push((key.clone(), value.clone()));
            }
            HookAction::Custom { id, .. } => {
                tracing::warn!(
                    target: "hook",
                    "Custom session hook '{}' for {:?} — not yet implemented",
                    id,
                    event
                );
            }
            HookAction::Http {
                url,
                headers,
                timeout_secs,
            } => {
                let payload = serde_json::json!({
                    "hook_event": format!("{:?}", event).to_lowercase(),
                    "session_id": session_id,
                    "user_message": user_message,
                });
                if let Some(body) = http_post_json(url, headers, &payload, *timeout_secs).await
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(ctx) = v.get("context").and_then(|c| c.as_str())
                {
                    contexts.push(ctx.to_string());
                }
            }
        }
    }

    if !contexts.is_empty() {
        output.context = Some(contexts.join("\n"));
    }
    output
}

/// Run a shell command for a session event hook.
async fn run_shell_session_hook(
    command: &str,
    event: SessionEvent,
    session_id: &str,
    user_message: Option<&str>,
    timeout_secs: u32,
) -> Option<SessionHookOutput> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let input = serde_json::json!({
        "hook_event": event,
        "session_id": session_id,
        "user_message": user_message,
    });

    let mut child = match Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "hook", "Failed to spawn session hook '{}': {}", command, e);
            return None;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
        drop(stdin);
    }

    let mut stdout_handle = child.stdout.take();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);

    // Read stdout first (capped) to prevent pipe-full deadlock, then wait.
    let read_fut = async {
        let buf = match stdout_handle.as_mut() {
            Some(s) => read_capped(s).await,
            None => Vec::new(),
        };
        let status = child.wait().await;
        (buf, status)
    };

    match tokio::time::timeout(timeout, read_fut).await {
        Ok((buf, Ok(status))) if status.success() => Some(parse_session_hook_output(&buf)),
        Ok((_, Ok(status))) => {
            tracing::warn!(
                target: "hook",
                "Session hook '{}' exited with status {}",
                command,
                status.code().unwrap_or(-1)
            );
            None
        }
        Ok((_, Err(e))) => {
            tracing::warn!(target: "hook", "Session hook I/O error for '{}': {}", command, e);
            None
        }
        Err(_) => {
            let _ = child.kill().await;
            tracing::warn!(
                target: "hook",
                "Session hook '{}' timed out after {}s",
                command,
                timeout_secs
            );
            None
        }
    }
}

/// Parse stdout from a session event hook.
///
/// Expected JSON: `{"context": "...", "env": {"KEY": "VALUE"}}`
/// Plain text stdout is treated as context.
fn parse_session_hook_output(stdout: &[u8]) -> SessionHookOutput {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return SessionHookOutput::default();
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let context = v.get("context").and_then(|c| c.as_str()).map(String::from);
        let env_vars = v
            .get("env")
            .and_then(|e| e.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|val| (k.clone(), val.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        SessionHookOutput { context, env_vars }
    } else {
        // Plain text → treat as context
        SessionHookOutput {
            context: Some(trimmed.to_string()),
            env_vars: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Skill lifecycle hook tests ──────────────────────────────────────

    #[test]
    fn hooks_empty_check() {
        let h = SkillHooks::default();
        assert!(h.is_empty());

        let h = SkillHooks {
            pre_invoke: vec![HookAction::Shell {
                command: "echo test".into(),
            }],
            ..Default::default()
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hook_action_serde_roundtrip() {
        let actions = vec![
            HookAction::Shell {
                command: "echo hello".into(),
            },
            HookAction::SetEnv {
                key: "FOO".into(),
                value: "bar".into(),
            },
            HookAction::Custom {
                id: "my-hook".into(),
                config: Some(serde_json::json!({"key": "value"})),
            },
        ];

        let json = serde_json::to_string(&actions).unwrap();
        let parsed: Vec<HookAction> = serde_json::from_str(&json).unwrap();
        assert_eq!(actions, parsed);
    }

    #[test]
    fn hooks_all_phases_non_empty() {
        let h = SkillHooks {
            pre_invoke: vec![HookAction::Shell {
                command: "before".into(),
            }],
            post_invoke: vec![HookAction::SetEnv {
                key: "K".into(),
                value: "V".into(),
            }],
            on_error: vec![HookAction::Custom {
                id: "cleanup".into(),
                config: None,
            }],
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hooks_only_on_error_non_empty() {
        let h = SkillHooks {
            on_error: vec![HookAction::Shell {
                command: "notify".into(),
            }],
            ..Default::default()
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hook_action_shell_deserialize_from_json() {
        let json = r#"{"type": "shell", "command": "echo test"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::Shell {
                command: "echo test".into()
            }
        );
    }

    #[test]
    fn hook_action_set_env_deserialize_from_json() {
        let json = r#"{"type": "set_env", "key": "PATH", "value": "/usr/bin"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::SetEnv {
                key: "PATH".into(),
                value: "/usr/bin".into()
            }
        );
    }

    #[test]
    fn hook_action_custom_no_config() {
        let json = r#"{"type": "custom", "id": "webhook"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::Custom {
                id: "webhook".into(),
                config: None
            }
        );
    }

    #[test]
    fn skill_hooks_yaml_roundtrip() {
        let hooks = SkillHooks {
            pre_invoke: vec![
                HookAction::Shell {
                    command: "make lint".into(),
                },
                HookAction::SetEnv {
                    key: "SKILL_ACTIVE".into(),
                    value: "1".into(),
                },
            ],
            post_invoke: vec![HookAction::Shell {
                command: "echo done".into(),
            }],
            on_error: vec![],
        };

        let yaml = serde_yaml::to_string(&hooks).unwrap();
        let parsed: SkillHooks = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(hooks, parsed);
    }

    #[test]
    fn lifecycle_event_variants_constructible() {
        let events = [
            SkillLifecycleEvent::Discovered {
                name: "test".into(),
                source: super::super::manifest::SkillSourceKind::Local,
            },
            SkillLifecycleEvent::Loaded {
                name: "test".into(),
            },
            SkillLifecycleEvent::Activated {
                name: "test".into(),
                trigger: "src/main.rs".into(),
            },
            SkillLifecycleEvent::Invoked {
                name: "test".into(),
                context: super::super::manifest::ExecutionContext::Inline,
            },
            SkillLifecycleEvent::Completed {
                name: "test".into(),
                tokens_used: 1000,
                turns: 3,
            },
            SkillLifecycleEvent::Failed {
                name: "test".into(),
                error: "timeout".into(),
            },
        ];
        assert_eq!(events.len(), 6);
    }

    // ── Glob matching tests ─────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("bash", "bash"));
        assert!(!glob_match("bash", "read_file"));
    }

    #[test]
    fn glob_wildcard_star() {
        assert!(glob_match("read_*", "read_file"));
        assert!(glob_match("read_*", "read_dir"));
        assert!(!glob_match("read_*", "write_file"));
    }

    #[test]
    fn glob_wildcard_question() {
        assert!(glob_match("git_?", "git_a"));
        assert!(!glob_match("git_?", "git_ab"));
    }

    #[test]
    fn glob_star_matches_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_complex_pattern() {
        assert!(glob_match("*file*", "read_file_contents"));
        assert!(glob_match("git_*_*", "git_log_search"));
        assert!(!glob_match("git_*_*", "git_status"));
    }

    // ── Tool event hook tests ───────────────────────────────────────────

    #[test]
    fn tool_event_hook_matches_exact() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "check".into(),
            },
            timeout_secs: 10,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        };
        assert!(hook.matches_tool("bash"));
        assert!(!hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_matches_glob() {
        let hook = ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "write_*".into(),
            action: HookAction::Shell {
                command: "lint".into(),
            },
            timeout_secs: 10,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        };
        assert!(hook.matches_tool("write_file"));
        assert!(hook.matches_tool("write_new_file"));
        assert!(!hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_empty_matcher_matches_all() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: String::new(),
            action: HookAction::Shell {
                command: "log".into(),
            },
            timeout_secs: 10,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        };
        assert!(hook.matches_tool("bash"));
        assert!(hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_serde_roundtrip() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "echo pre".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        };
        let json = serde_json::to_string(&hook).unwrap();
        let parsed: ToolEventHook = serde_json::from_str(&json).unwrap();
        assert_eq!(hook, parsed);
    }

    #[test]
    fn registry_matching_filters_by_event_and_tool() {
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "pre-bash".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PostToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "post-bash".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "read_*".into(),
                action: HookAction::Shell {
                    command: "pre-read".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);
        assert_eq!(registry.len(), 3);

        let pre_bash = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(pre_bash.len(), 1);
        assert_eq!(
            pre_bash[0].action,
            HookAction::Shell {
                command: "pre-bash".into()
            }
        );

        let post_bash = registry.matching(ToolEventKind::PostToolUse, "bash");
        assert_eq!(post_bash.len(), 1);

        let pre_read = registry.matching(ToolEventKind::PreToolUse, "read_file");
        assert_eq!(pre_read.len(), 1);

        let pre_write = registry.matching(ToolEventKind::PreToolUse, "write_file");
        assert!(pre_write.is_empty());
    }

    #[test]
    fn registry_empty_returns_no_matches() {
        let registry = ToolEventHookRegistry::default();
        assert!(registry.is_empty());
        assert!(
            registry
                .matching(ToolEventKind::PreToolUse, "bash")
                .is_empty()
        );
    }

    #[test]
    fn pre_tool_decision_variants() {
        let allow = PreToolDecision::Allow;
        let ctx = PreToolDecision::AllowWithContext("extra info".into());
        let block = PreToolDecision::Block("denied".into());

        assert_eq!(allow, PreToolDecision::Allow);
        assert_eq!(ctx, PreToolDecision::AllowWithContext("extra info".into()));
        assert_eq!(block, PreToolDecision::Block("denied".into()));
    }

    // ── E2E: full hook pipeline ─────────────────────────────────────────

    #[test]
    fn e2e_hooks_json_config_to_registry_to_decisions() {
        // Simulates the full hook lifecycle:
        // 1. Load hooks from JSON config
        // 2. Build registry
        // 3. Match against tool calls
        // 4. Produce pre-tool decisions

        // Step 1: Parse hook config from JSON (as would come from .astra/hooks.json)
        let config_json = r#"[
            {
                "event": "pre_tool_use",
                "matcher": "bash",
                "action": {"type": "shell", "command": "echo 'checking bash command'"},
                "timeout_secs": 5
            },
            {
                "event": "post_tool_use",
                "matcher": "write_*",
                "action": {"type": "shell", "command": "make lint"},
                "timeout_secs": 30
            },
            {
                "event": "pre_tool_use",
                "matcher": "*",
                "action": {"type": "custom", "id": "audit_log"},
                "timeout_secs": 2
            }
        ]"#;
        let hooks: Vec<ToolEventHook> = serde_json::from_str(config_json).unwrap();
        assert_eq!(hooks.len(), 3);

        // Step 2: Build registry
        let registry = ToolEventHookRegistry::new(hooks);
        assert_eq!(registry.len(), 3);

        // Step 3: Match against various tool calls
        // bash: should match both the "bash" hook and the "*" hook
        let pre_bash = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(pre_bash.len(), 2);
        assert_eq!(pre_bash[0].matcher, "bash");
        assert_eq!(pre_bash[1].matcher, "*");

        // write_file: should match post_tool_use "write_*" and pre_tool_use "*"
        let post_write = registry.matching(ToolEventKind::PostToolUse, "write_file");
        assert_eq!(post_write.len(), 1);
        assert_eq!(post_write[0].matcher, "write_*");

        let pre_write = registry.matching(ToolEventKind::PreToolUse, "write_file");
        assert_eq!(pre_write.len(), 1); // only the "*" catch-all
        assert_eq!(pre_write[0].matcher, "*");

        // read_file: only the catch-all "*" for pre_tool_use
        let pre_read = registry.matching(ToolEventKind::PreToolUse, "read_file");
        assert_eq!(pre_read.len(), 1);

        // no post_tool_use hooks for read_file
        let post_read = registry.matching(ToolEventKind::PostToolUse, "read_file");
        assert!(post_read.is_empty());

        // Step 4: Verify decision flow
        // Simulate: a pre-tool hook returns "block" for dangerous bash command
        let bash_hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        let first_hook = bash_hooks[0];
        match &first_hook.action {
            HookAction::Shell { command } => {
                assert!(command.contains("checking bash"));
                // In a real system: run the shell command, parse its JSON output
                // to get allow/block decision. Here we verify the config is correct.
            }
            _ => panic!("expected shell action"),
        }

        // The catch-all audit hook should be a Custom action
        let audit_hook = bash_hooks[1];
        match &audit_hook.action {
            HookAction::Custom { id, .. } => assert_eq!(id, "audit_log"),
            _ => panic!("expected custom action"),
        }
    }

    #[test]
    fn e2e_hooks_yaml_config_roundtrip() {
        let hooks = vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "validate-command.sh".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PostToolUse,
                matcher: "write_*".into(),
                action: HookAction::Shell {
                    command: "run-linter.sh".into(),
                },
                timeout_secs: 30,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ];

        // Roundtrip through YAML (skill frontmatter format)
        let yaml = serde_yaml::to_string(&hooks).unwrap();
        let parsed: Vec<ToolEventHook> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(hooks, parsed);

        // Roundtrip through JSON (project config format)
        let json = serde_json::to_string_pretty(&hooks).unwrap();
        let parsed: Vec<ToolEventHook> = serde_json::from_str(&json).unwrap();
        assert_eq!(hooks, parsed);
    }

    #[test]
    fn e2e_multiple_matchers_priority_order() {
        // Hooks are evaluated in config order — most specific first, catch-all last
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "specific-bash-check".into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bas*".into(),
                action: HookAction::Shell {
                    command: "glob-bash-check".into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: "catch-all".into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let matches = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(matches.len(), 3);
        // Order preserved from config
        match &matches[0].action {
            HookAction::Shell { command } => assert_eq!(command, "specific-bash-check"),
            _ => panic!(),
        }
        match &matches[1].action {
            HookAction::Shell { command } => assert_eq!(command, "glob-bash-check"),
            _ => panic!(),
        }
        match &matches[2].action {
            HookAction::Shell { command } => assert_eq!(command, "catch-all"),
            _ => panic!(),
        }
    }

    // ── E2E: async hook execution with real shell commands ──────────

    #[tokio::test]
    async fn e2e_pre_hook_shell_allow() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "allow"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_block() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "block", "reason": "rm -rf detected"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Block("rm -rf detected".into()));
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_allow_with_context() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "allow", "context": "hook injected info"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(
            decision,
            PreToolDecision::AllowWithContext("hook injected info".into())
        );
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_exit_nonzero_blocks() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "exit 1".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        match decision {
            PreToolDecision::Block(reason) => assert!(reason.contains("exited with status")),
            _ => panic!("expected Block, got {:?}", decision),
        }
    }

    #[tokio::test]
    async fn e2e_pre_hook_no_match_allows() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "block", "reason": "nope"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_reads_tool_input() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: r#"INPUT=$(cat); TOOL=$(echo "$INPUT" | grep -o '"tool_name":"[^"]*"' | head -1); if echo "$TOOL" | grep -q 'write_file'; then echo '{"decision":"block","reason":"writes blocked"}'; else echo '{"decision":"allow"}'; fi"#.into(),
            },
            timeout_secs: 5,
        is_async: false,
        condition: None,
        once: false,
        priority: 0,
        }]);

        let allow = evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(allow, PreToolDecision::Allow);

        let block = evaluate_pre_tool_hooks(&registry, "write_file", &serde_json::json!({})).await;
        assert_eq!(block, PreToolDecision::Block("writes blocked".into()));
    }

    #[tokio::test]
    async fn e2e_post_hook_shell_modifies_output() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"output": "modified output"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let result =
            evaluate_post_tool_hooks(&registry, "bash", &serde_json::json!({}), "original output")
                .await;
        assert_eq!(result, Some("modified output".into()));
    }

    #[tokio::test]
    async fn e2e_post_hook_shell_no_modification() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let result =
            evaluate_post_tool_hooks(&registry, "bash", &serde_json::json!({}), "original output")
                .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn e2e_pre_hook_empty_registry_allows() {
        let registry = ToolEventHookRegistry::default();
        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_empty_output_allows() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: "true".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_multiple_context_accumulates() {
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: r#"echo '{"decision":"allow","context":"hook1 info"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: r#"echo '{"decision":"allow","context":"hook2 info"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let decision = evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        match decision {
            PreToolDecision::AllowWithContext(ctx) => {
                assert!(ctx.contains("hook1 info"));
                assert!(ctx.contains("hook2 info"));
            }
            _ => panic!("expected AllowWithContext, got {:?}", decision),
        }
    }

    // ── Config loading tests ────────────────────────────────────────

    #[test]
    fn load_hooks_json_array() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"[
            {
                "event": "pre_tool_use",
                "matcher": "bash",
                "action": {"type": "shell", "command": "check-bash.sh"},
                "timeout_secs": 5
            }
        ]"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
        let hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn load_hooks_json_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"{
            "hooks": [
                {
                    "event": "post_tool_use",
                    "matcher": "write_*",
                    "action": {"type": "shell", "command": "lint.sh"}
                }
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let yaml = r#"
- event: pre_tool_use
  matcher: "*"
  action:
    type: shell
    command: audit-log.sh
  timeout_secs: 2
"#;
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_yaml_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let yaml = r#"
hooks:
  - event: pre_tool_use
    matcher: bash
    action:
      type: shell
      command: validate.sh
"#;
        std::fs::write(astra.join("hooks.yml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_no_astra_dir() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_no_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".astra")).unwrap();
        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        std::fs::write(astra.join("hooks.json"), "not valid json").unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_json_preferred_over_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();

        let json = r#"[
            {"event": "pre_tool_use", "matcher": "a", "action": {"type": "shell", "command": "a"}},
            {"event": "pre_tool_use", "matcher": "b", "action": {"type": "shell", "command": "b"}}
        ]"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let yaml =
            "- event: pre_tool_use\n  matcher: c\n  action:\n    type: shell\n    command: c\n";
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 2); // JSON takes precedence
    }

    #[test]
    fn load_hooks_json_default_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();

        let json = r#"{
            "default_timeout_secs": 30,
            "hooks": [
                {"event": "pre_tool_use", "matcher": "bash", "action": {"type": "shell", "command": "check.sh"}},
                {"event": "pre_tool_use", "matcher": "edit", "action": {"type": "shell", "command": "lint.sh"}, "timeout_secs": 5}
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 2);
        let hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(hooks[0].timeout_secs, 30); // inherited global default
        let hooks = registry.matching(ToolEventKind::PreToolUse, "edit");
        assert_eq!(hooks[0].timeout_secs, 5); // explicit override preserved
    }

    #[test]
    fn load_hooks_yaml_default_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();

        let yaml = r#"
default_timeout_secs: 20
hooks:
  - event: pre_tool_use
    matcher: bash
    action:
      type: shell
      command: check.sh
  - event: post_tool_use
    matcher: "*"
    action:
      type: shell
      command: log.sh
    timeout_secs: 3
"#;
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 2);
        let hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(hooks[0].timeout_secs, 20); // inherited global default
        let hooks = registry.matching(ToolEventKind::PostToolUse, "anything");
        assert_eq!(hooks[0].timeout_secs, 3); // explicit override preserved
    }

    #[test]
    fn apply_default_timeout_no_override() {
        let hooks = vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: "a".into(),
            },
            timeout_secs: 10,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }];
        // No default → no change
        let result = apply_default_timeout(hooks, None);
        assert_eq!(result[0].timeout_secs, 10);
    }

    // ── Session event hook tests ────────────────────────────────────

    #[test]
    fn session_event_serde_roundtrip() {
        let hook = SessionEventHook {
            event: SessionEvent::SessionStart,
            action: HookAction::Shell {
                command: "echo hello".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        };
        let json = serde_json::to_string(&hook).unwrap();
        let parsed: SessionEventHook = serde_json::from_str(&json).unwrap();
        assert_eq!(hook, parsed);
    }

    #[test]
    fn session_event_all_variants_deserialize() {
        for (name, expected) in [
            ("session_start", SessionEvent::SessionStart),
            ("session_end", SessionEvent::SessionEnd),
            ("user_prompt_submit", SessionEvent::UserPromptSubmit),
            ("subagent_start", SessionEvent::SubagentStart),
        ] {
            let json = format!(
                r#"{{"event":"{}","action":{{"type":"shell","command":"x"}}}}"#,
                name
            );
            let hook: SessionEventHook = serde_json::from_str(&json).unwrap();
            assert_eq!(hook.event, expected);
        }
    }

    #[test]
    fn session_registry_matching() {
        let registry = SessionEventHookRegistry::new(vec![
            SessionEventHook {
                event: SessionEvent::SessionStart,
                action: HookAction::Shell {
                    command: "greet".into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            SessionEventHook {
                event: SessionEvent::SessionEnd,
                action: HookAction::Shell {
                    command: "cleanup".into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            SessionEventHook {
                event: SessionEvent::SessionStart,
                action: HookAction::SetEnv {
                    key: "SESSION".into(),
                    value: "1".into(),
                },
                timeout_secs: 10,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.matching(SessionEvent::SessionStart).len(), 2);
        assert_eq!(registry.matching(SessionEvent::SessionEnd).len(), 1);
        assert_eq!(registry.matching(SessionEvent::UserPromptSubmit).len(), 0);
    }

    #[test]
    fn session_registry_empty() {
        let registry = SessionEventHookRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.matching(SessionEvent::SessionStart).is_empty());
    }

    #[test]
    fn parse_session_hook_output_json_context() {
        let stdout = br#"{"context": "Welcome back!"}"#;
        let out = parse_session_hook_output(stdout);
        assert_eq!(out.context.as_deref(), Some("Welcome back!"));
        assert!(out.env_vars.is_empty());
    }

    #[test]
    fn parse_session_hook_output_json_env() {
        let stdout = br#"{"context": "hi", "env": {"FOO": "bar", "BAZ": "qux"}}"#;
        let out = parse_session_hook_output(stdout);
        assert_eq!(out.context.as_deref(), Some("hi"));
        assert_eq!(out.env_vars.len(), 2);
        assert!(out.env_vars.contains(&("FOO".into(), "bar".into())));
    }

    #[test]
    fn parse_session_hook_output_plain_text() {
        let stdout = b"Hello, xupeng!";
        let out = parse_session_hook_output(stdout);
        assert_eq!(out.context.as_deref(), Some("Hello, xupeng!"));
        assert!(out.env_vars.is_empty());
    }

    #[test]
    fn parse_session_hook_output_empty() {
        let out = parse_session_hook_output(b"");
        assert!(out.context.is_none());
        assert!(out.env_vars.is_empty());
    }

    #[test]
    fn load_session_hooks_from_json() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"{
            "hooks": [],
            "session_hooks": [
                {
                    "event": "session_start",
                    "action": {"type": "shell", "command": "echo greeting"},
                    "timeout_secs": 3
                }
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_session_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.matching(SessionEvent::SessionStart).len(), 1);
    }

    #[test]
    fn load_session_hooks_from_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let yaml = r#"
hooks: []
session_hooks:
  - event: session_start
    action:
      type: shell
      command: echo hi
  - event: session_end
    action:
      type: shell
      command: echo bye
"#;
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_session_event_hooks(dir.path());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn load_session_hooks_legacy_array_returns_empty() {
        // Legacy format (plain array) has no session_hooks key → empty
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"[
            {"event": "pre_tool_use", "matcher": "bash", "action": {"type": "shell", "command": "x"}}
        ]"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_session_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_session_hooks_default_timeout_applied() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"{
            "default_timeout_secs": 30,
            "hooks": [],
            "session_hooks": [
                {"event": "session_start", "action": {"type": "shell", "command": "greet"}},
                {"event": "session_end", "action": {"type": "shell", "command": "bye"}, "timeout_secs": 5}
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_session_event_hooks(dir.path());
        let start = registry.matching(SessionEvent::SessionStart);
        assert_eq!(
            start[0].timeout_secs, 30,
            "should inherit default_timeout_secs"
        );
        let end = registry.matching(SessionEvent::SessionEnd);
        assert_eq!(
            end[0].timeout_secs, 5,
            "explicit timeout should be preserved"
        );
    }

    #[test]
    fn load_both_tool_and_session_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"{
            "hooks": [
                {"event": "pre_tool_use", "matcher": "bash", "action": {"type": "shell", "command": "check"}}
            ],
            "session_hooks": [
                {"event": "session_start", "action": {"type": "shell", "command": "greet"}}
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let tool_reg = load_tool_event_hooks(dir.path());
        let session_reg = load_session_event_hooks(dir.path());
        assert_eq!(tool_reg.len(), 1);
        assert_eq!(session_reg.len(), 1);
    }

    // ── E2E: session hook execution ─────────────────────────────────

    #[tokio::test]
    async fn e2e_session_start_hook_returns_context() {
        let registry = SessionEventHookRegistry::new(vec![SessionEventHook {
            event: SessionEvent::SessionStart,
            action: HookAction::Shell {
                command: r#"echo '{"context": "Welcome back, user!"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let output = evaluate_session_hooks(
            &registry,
            SessionEvent::SessionStart,
            "test-session",
            Some("hello"),
        )
        .await;
        assert_eq!(output.context.as_deref(), Some("Welcome back, user!"));
    }

    #[tokio::test]
    async fn e2e_session_hook_set_env() {
        let registry = SessionEventHookRegistry::new(vec![SessionEventHook {
            event: SessionEvent::SessionStart,
            action: HookAction::SetEnv {
                key: "GREETING".into(),
                value: "done".into(),
            },
            timeout_secs: 10,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let output =
            evaluate_session_hooks(&registry, SessionEvent::SessionStart, "s1", None).await;
        assert!(output.context.is_none());
        assert_eq!(output.env_vars, vec![("GREETING".into(), "done".into())]);
    }

    #[tokio::test]
    async fn e2e_session_hook_no_match_returns_default() {
        let registry = SessionEventHookRegistry::new(vec![SessionEventHook {
            event: SessionEvent::SessionEnd,
            action: HookAction::Shell {
                command: "echo bye".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let output =
            evaluate_session_hooks(&registry, SessionEvent::SessionStart, "s1", None).await;
        assert!(output.context.is_none());
        assert!(output.env_vars.is_empty());
    }

    #[tokio::test]
    async fn e2e_session_hook_multiple_contexts_joined() {
        let registry = SessionEventHookRegistry::new(vec![
            SessionEventHook {
                event: SessionEvent::SessionStart,
                action: HookAction::Shell {
                    command: r#"echo '{"context": "hook1"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
            SessionEventHook {
                event: SessionEvent::SessionStart,
                action: HookAction::Shell {
                    command: r#"echo '{"context": "hook2"}'"#.into(),
                },
                timeout_secs: 5,
                is_async: false,
                condition: None,
                once: false,
                priority: 0,
            },
        ]);

        let output =
            evaluate_session_hooks(&registry, SessionEvent::SessionStart, "s1", Some("hi")).await;
        let ctx = output.context.unwrap();
        assert!(ctx.contains("hook1"));
        assert!(ctx.contains("hook2"));
    }

    #[tokio::test]
    async fn e2e_session_hook_failed_command_skipped() {
        let registry = SessionEventHookRegistry::new(vec![SessionEventHook {
            event: SessionEvent::SessionStart,
            action: HookAction::Shell {
                command: "exit 1".into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);

        let output =
            evaluate_session_hooks(&registry, SessionEvent::SessionStart, "s1", None).await;
        // Failed hook is skipped, no context
        assert!(output.context.is_none());
    }

    #[tokio::test]
    async fn e2e_session_hook_empty_registry() {
        let registry = SessionEventHookRegistry::default();
        let output =
            evaluate_session_hooks(&registry, SessionEvent::SessionStart, "s1", None).await;
        assert!(output.context.is_none());
        assert!(output.env_vars.is_empty());
    }
}
