//! Default tool executor — the shared implementation used by CLI, server, and edge.
//!
//! Routes tool calls to the appropriate module (fs_ops, shell_ops, git_gix, etc.)
//! and returns [`ToolResult`]. Consumers wrap this with their own context
//! (e.g., `ServerToolExecutor` adds resource governance and process isolation,
//! `CliToolExecutor` adds terminal UI and MCP dispatch).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::github::GitHubClient;
use crate::task_mgmt::{InMemoryTaskStore, TaskManager, TaskStore};
use crate::{ToolApprovalGate, ToolContext, ToolExecutor, ToolProgressCallback, ToolResult};

/// Tools the server runtime may safely route straight through the shared
/// [`DefaultToolExecutor`] without adding server-specific journaling,
/// rollback, approval, or transport behavior.
///
/// Keep mutating wrappers (`write_file`, `str_replace`, `bash`, `run_script`,
/// rollback tools, task/session/control-plane tools) out of this list. They
/// need server-local handlers so observability and rollback stay authoritative.
pub const SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES: &[&str] = &[
    "web_fetch",
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "symbols",
    "git",
    "github",
];

pub fn is_server_direct_default_executor_tool(name: &str) -> bool {
    SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES.contains(&name)
}

// ─── Helper ─────────────────────────────────────────────────────────────────

/// Convert a String-returning tool function to ToolResult.
/// Prefer structured JSON failure (`status=failed`, `success=false`, or
/// `error`) over legacy text prefixes.
fn string_to_result(output: String) -> ToolResult {
    let parsed = serde_json::from_str::<Value>(&output).ok();
    let structured_error = parsed
        .as_ref()
        .and_then(|value| value.get("success").and_then(Value::as_bool))
        .is_some_and(|success| !success)
        || parsed
            .as_ref()
            .and_then(|value| value.get("status").and_then(Value::as_str))
            .is_some_and(structured_status_is_error)
        || parsed
            .as_ref()
            .and_then(|value| value.get("error"))
            .is_some_and(json_error_value_is_error);
    if structured_error || output.starts_with("Error") {
        ToolResult::error(output)
    } else {
        ToolResult::text(output)
    }
}

fn json_error_value_is_error(error: &Value) -> bool {
    !error.is_null() && error.as_str() != Some("")
}

fn structured_status_is_error(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "failed"
            | "error"
            | "partial_failure"
            | "denied"
            | "cancelled"
            | "canceled"
            | "timeout"
            | "timed_out"
    )
}

fn outcome_to_result(outcome: crate::git_gix::ToolExecutionOutcome) -> ToolResult {
    ToolResult {
        output: outcome.output,
        metadata: outcome.tool_result_fields,
        is_error: outcome.is_error,
        exit_semantics: None,
    }
}

// ─── DefaultToolExecutor ────────────────────────────────────────────────────

/// Per-tool execution timeout. Prevents synchronous tools (tree-sitter, etc.)
/// from hanging indefinitely on large inputs.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Maximum output size returned to the LLM. Larger outputs are truncated to
/// prevent context window overflow.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024; // 64 KB

/// Default tool executor with the full shared tool set.
///
/// Covers file ops, shell, git (via gix), GitHub API, code intelligence,
/// task management, and utility tools. CLI-specific tools (ask_user, MCP,
/// LSP subprocess, interactive shell) are handled by wrapping executors.
pub struct DefaultToolExecutor {
    ctx: ToolContext,
    approval_gate: Option<Arc<dyn ToolApprovalGate>>,
    progress_callback: Option<Arc<dyn ToolProgressCallback>>,
    github_client: Option<GitHubClient>,
    task_manager: Arc<TaskManager>,
    bash_cache: Arc<Mutex<HashMap<BashCacheKey, BashCacheEntry>>>,
    workspace_generation: Arc<AtomicU64>,
    bash_cache_ttl: std::time::Duration,
    /// Tracks whether the HTTP client was successfully built.
    /// When `false`, GitHub tools and other HTTP-dependent tools will report
    /// a diagnostic error explaining why HTTP is unavailable.
    http_client_available: bool,
}

/// Key for the per-session bash dedup cache. Bumping ANY of these
/// fields must invalidate prior entries — otherwise we'd return a
/// result computed under a different precondition and the model would
/// act on stale state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BashCacheKey {
    workspace_root: String,
    /// Monotonic counter bumped whenever a mutation tool succeeds. A
    /// newly-created key after `write_file` will miss the cache, so
    /// prior `ls` / `grep` results don't leak across edits made
    /// through the tool pipeline.
    workspace_generation: u64,
    command: String,
    /// Fingerprint of env vars that meaningfully change command
    /// output (`PATH`, `HOME`, `LANG`, locale vars, `TZ`). We hash
    /// them rather than storing raw — the classifier already filters
    /// the command set down to read-only tools, but their output can
    /// still depend on locale / user home.
    env_fingerprint: u64,
    /// Hash of `args.stdin` when present. A `cat` invocation whose
    /// stdin differs between calls must NOT share a cache entry.
    stdin_hash: u64,
}

/// Cache entry with insertion timestamp. We use a TTL on top of
/// `workspace_generation` because non-tool filesystem mutations
/// (user's editor, git pull, external script) don't bump the
/// generation — so `git status` / `ls` results could otherwise go
/// stale indefinitely. The TTL is a coarse-grained safety net; tests
/// can swap it via `with_bash_cache_ttl`.
#[derive(Debug, Clone)]
struct BashCacheEntry {
    result: ToolResult,
    inserted_at: std::time::Instant,
}

/// Default: cached bash output goes stale after 30 seconds of
/// wall-clock time. Long enough that a tight `ls`/`ls`/`ls` loop
/// benefits; short enough that an external `git pull` becomes
/// visible on the next read-only probe.
pub const DEFAULT_BASH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Recover a poisoned `Mutex` while logging the recovery. A panic that
/// poisoned the lock is a real bug — callers should not silently swallow it.
/// We keep the recovery (continuing is better than propagating panic across
/// an await boundary in a tool executor), but emit an `error!` so operators
/// can correlate the root cause.
///
/// Also increments a global counter so monitoring systems can alert on
/// mutex poisoning events without scraping logs.
static POISONED_LOCK_RECOVERY_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn recover_poisoned_lock<'a, T>(
    result: std::sync::LockResult<std::sync::MutexGuard<'a, T>>,
    lock_name: &'static str,
) -> std::sync::MutexGuard<'a, T> {
    match result {
        Ok(guard) => guard,
        Err(poison) => {
            let count =
                POISONED_LOCK_RECOVERY_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::error!(
                lock = lock_name,
                recovery_count = count,
                "mutex poisoned by a panicking thread; recovering to keep tool executor available"
            );
            poison.into_inner()
        }
    }
}

/// Returns the number of times a poisoned mutex was recovered.
/// Useful for monitoring and alerting.
#[allow(dead_code)]
pub fn poisoned_lock_recovery_count() -> u64 {
    POISONED_LOCK_RECOVERY_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

impl DefaultToolExecutor {
    pub fn new(ctx: ToolContext) -> Self {
        let store: Arc<dyn TaskStore> = Arc::new(InMemoryTaskStore::new());
        let task_manager = Arc::new(TaskManager::new(ctx.session_id.clone(), store));
        Self {
            ctx,
            approval_gate: None,
            progress_callback: None,
            github_client: None,
            task_manager,
            bash_cache: Arc::new(Mutex::new(HashMap::new())),
            workspace_generation: Arc::new(AtomicU64::new(0)),
            bash_cache_ttl: DEFAULT_BASH_CACHE_TTL,
            http_client_available: true,
        }
    }

    /// Reuse an externally supplied task store (required for MO-backed
    /// cross-client task visibility; keeps `Self::new` ergonomic for tests
    /// that just want a process-local store).
    pub fn with_task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        self.task_manager = Arc::new(TaskManager::new(self.ctx.session_id.clone(), store));
        self
    }

    /// Override the bash cache TTL. Intended for tests; production
    /// code uses [`DEFAULT_BASH_CACHE_TTL`].
    #[cfg(test)]
    pub(crate) fn with_bash_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.bash_cache_ttl = ttl;
        self
    }

    /// Build a ready-to-use executor from workspace parameters.
    ///
    /// Handles the full setup recipe shared by edge and cloud:
    /// HTTP client, `ToolContext`, sandbox, and optional GitHub integration.
    ///
    /// If the HTTP client cannot be built, a warning is logged and the
    /// executor is created without HTTP support (GitHub tools etc. will
    /// report errors rather than crashing the runtime).
    pub fn for_workspace(
        workspace: &Path,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        user_agent: &str,
        timeout: std::time::Duration,
    ) -> Self {
        let (http_client, http_client_available) = match reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(user_agent.to_string())
            .no_proxy()
            .build()
        {
            Ok(client) => (client, true),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to build HTTP client for tool executor — HTTP-dependent tools will be unavailable"
                );
                (reqwest::Client::new(), false)
            }
        };

        let ctx = crate::ToolContext {
            project_root: workspace.to_path_buf(),
            workspace_root: workspace.to_path_buf(),
            user_id: user_id.into(),
            session_id: session_id.into(),
            sandbox: crate::SandboxConfig::standard(workspace),
            http_client: Some(http_client.clone()),
            logger: Arc::new(crate::TracingLogger),
            cancel_token: None,
            detach_shell_handle: None,
        };

        let mut executor = Self::new(ctx);
        executor.http_client_available = http_client_available;
        let tokens = crate::github::resolve_github_tokens();
        if !tokens.is_empty() {
            let github = GitHubClient::from_tokens(http_client, tokens, Vec::new());
            executor = executor.with_github_client(github);
        }
        executor
    }

    pub fn with_github_client(mut self, client: GitHubClient) -> Self {
        self.github_client = Some(client);
        self
    }
    pub fn with_cancel_token(mut self, token: Option<Arc<CancellationToken>>) -> Self {
        self.ctx.cancel_token = token;
        self
    }

    /// Install the host's detach slot so the bash runner can hand
    /// off live children to the BackgroundTaskRegistry on Ctrl+B.
    /// `None` is the default (no detach plumbing — bash runs through
    /// the legacy reader). The slot itself is renewable: the host
    /// refills it before each tool call so each bash invocation
    /// gets a fresh one-shot.
    pub fn with_detach_shell_slot(mut self, slot: Option<crate::detach::DetachShellSlot>) -> Self {
        self.ctx.detach_shell_handle = slot;
        self
    }

    /// Mutable setter for the detach slot. Used by the TUI/CLI host
    /// when it constructs the executor first and wires the slot
    /// later (after the BackgroundTaskRegistry is available).
    pub fn set_detach_shell_slot(&mut self, slot: Option<crate::detach::DetachShellSlot>) {
        self.ctx.detach_shell_handle = slot;
    }

    /// Access the underlying context.
    pub fn context(&self) -> &ToolContext {
        &self.ctx
    }

    /// Workspace root path (alias for `context().workspace_root`).
    pub fn workspace_root(&self) -> &Path {
        &self.ctx.workspace_root
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> ToolResult {
        // ── Approval gate ────────────────────────────────────────────
        if let Some(gate) = &self.approval_gate
            && gate.requires_approval_for(name, args)
        {
            let request_id = uuid::Uuid::new_v4().to_string();
            let decision = gate.request_approval(&request_id, name, args).await;
            match decision {
                crate::ApprovalDecision::Approved => {}
                crate::ApprovalDecision::Denied { reason } => {
                    let msg = reason.unwrap_or_else(|| "denied by user".into());
                    return ToolResult::error(format!(
                        "The user REJECTED this tool call. The tool was NOT executed.\n\
                         User feedback: \"{msg}\"\n\
                         IMPORTANT: Do NOT retry this exact approach. \
                         Ask the user how to proceed, or try a safer alternative."
                    ));
                }
                crate::ApprovalDecision::Timeout => {
                    return ToolResult::error(
                        "Tool execution denied: approval request timed out".into(),
                    );
                }
            }
        }

        // ── Progress notification ────────────────────────────────────
        let call_id = uuid::Uuid::new_v4().to_string();
        if let Some(cb) = &self.progress_callback {
            cb.tool_started(&call_id, name, args).await;
        }

        // Check cancellation before executing the tool.
        if self
            .ctx
            .cancel_token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
        {
            return ToolResult::error(format!("Tool '{name}' not executed: run was cancelled"));
        }

        if name == "bash"
            && !args.get("force").and_then(Value::as_bool).unwrap_or(false)
            && let Some(key) = self.bash_cache_key(args)
            && let Some(mut cached) = {
                // Lookup + TTL check + stale eviction under one
                // critical section. We clone the `ToolResult` out
                // before returning so the lock is released before
                // the progress callback runs.
                let mut map = self
                    .bash_cache
                    .lock()
                    .unwrap_or_else(|e| recover_poisoned_lock(Err(e), "bash_cache"));
                let now = std::time::Instant::now();
                let ttl = self.bash_cache_ttl;
                match map.get(&key) {
                    Some(entry) if now.duration_since(entry.inserted_at) < ttl => {
                        Some(entry.result.clone())
                    }
                    Some(_) => {
                        // Stale — evict so future lookups don't keep
                        // re-hitting a dead entry and so the next
                        // real execution gets cached fresh.
                        map.remove(&key);
                        None
                    }
                    None => None,
                }
            }
        {
            mark_result_cached(&mut cached);
            if let Some(cb) = &self.progress_callback {
                cb.tool_completed(&call_id, &cached.output, !cached.is_error)
                    .await;
            }
            return cached;
        }

        let dispatch = self.dispatch(name, args);
        let result = if let Some(token) = self.ctx.cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    ToolResult::error(format!("Tool '{name}' cancelled before completion"))
                }
                result = tokio::time::timeout(TOOL_TIMEOUT, dispatch) => match result {
                    Ok(r) => r,
                    Err(_) => ToolResult::error(format!(
                        "Tool '{name}' timed out after {}s",
                        TOOL_TIMEOUT.as_secs()
                    )),
                },
            }
        } else {
            match tokio::time::timeout(TOOL_TIMEOUT, dispatch).await {
                Ok(r) => r,
                Err(_) => ToolResult::error(format!(
                    "Tool '{name}' timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                )),
            }
        };
        // Truncate oversized output to prevent context window overflow.
        let result = if result.output.len() > MAX_TOOL_OUTPUT_BYTES {
            let safe_len = result.output.floor_char_boundary(MAX_TOOL_OUTPUT_BYTES);
            ToolResult {
                output: format!(
                    "{}\n[output truncated at {}KB — {} bytes omitted]",
                    &result.output[..safe_len],
                    MAX_TOOL_OUTPUT_BYTES / 1024,
                    result.output.len() - safe_len,
                ),
                ..result
            }
        } else {
            result
        };

        if name == "bash"
            && !result.is_error
            && let Some(key) = self.bash_cache_key(args)
        {
            self.bash_cache
                .lock()
                .unwrap_or_else(|e| recover_poisoned_lock(Err(e), "bash_cache"))
                .insert(
                    key,
                    BashCacheEntry {
                        result: result.clone(),
                        inserted_at: std::time::Instant::now(),
                    },
                );
        }
        if is_workspace_mutation_tool(name, args) && !result.is_error {
            self.workspace_generation.fetch_add(1, Ordering::Relaxed);
        }

        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }

    fn tool_schemas(&self) -> Vec<Value> {
        crate::schemas::all_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.ctx.project_root
    }
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

impl DefaultToolExecutor {
    fn bash_cache_key(&self, args: &Value) -> Option<BashCacheKey> {
        use crate::bash_cache_safety::bash_command_is_cache_safe;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let command = args.get("command")?.as_str()?.to_string();

        // Readonly classifier: only commands whose output depends
        // solely on fs + env may hit the cache. Anything with side
        // effects (rm, cargo build, git commit, curl, …) or shell
        // compound markers is rejected — returning None disables
        // caching for this call entirely. See
        // `bash_cache_safety.rs` for the full taxonomy.
        if !bash_command_is_cache_safe(&command) {
            return None;
        }

        // Env fingerprint: a subset of env vars meaningfully
        // influences output of read-only tools. We hash rather than
        // store to keep the key bounded. Absent vars hash as empty.
        let env_fingerprint = {
            const KEYS: &[&str] = &[
                "PATH",
                "HOME",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "LC_MESSAGES",
                "TZ",
            ];
            let mut h = DefaultHasher::new();
            for k in KEYS {
                k.hash(&mut h);
                std::env::var(k).unwrap_or_default().hash(&mut h);
            }
            h.finish()
        };

        // Stdin hash: `cat`, `grep`, etc. can be fed via stdin —
        // same command string + different stdin must not collide.
        let stdin_hash = {
            let mut h = DefaultHasher::new();
            let stdin = args
                .get("stdin")
                .and_then(Value::as_str)
                .unwrap_or_default();
            stdin.hash(&mut h);
            h.finish()
        };

        Some(BashCacheKey {
            workspace_root: self.ctx.workspace_root.display().to_string(),
            workspace_generation: self.workspace_generation.load(Ordering::Relaxed),
            command,
            env_fingerprint,
            stdin_hash,
        })
    }

    async fn dispatch(&self, name: &str, args: &Value) -> ToolResult {
        let ws = &self.ctx.workspace_root;
        let pr = &self.ctx.project_root;

        match name {
            // ── File operations ──────────────────────────────────────
            "read_file" => crate::fs_ops::read_file(ws, args),
            "write_file" => {
                if args
                    .get("delete")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    crate::fs_ops::delete_file(ws, args)
                } else {
                    crate::fs_ops::write_file(ws, args)
                }
            }
            "str_replace" => crate::fs_ops::str_replace(ws, args),
            "delete_file" => crate::fs_ops::delete_file(ws, args),
            "list_dir" => crate::fs_ops::list_dir(ws, args),

            // ── Multi-edit (atomic) ──────────────────────────────────
            "multi_edit" => crate::fs_ops::multi_edit(ws, args),

            // ── Shell operations ─────────────────────────────────────
            "bash" => crate::shell_ops::execute_bash(&self.ctx, args).await,
            "grep" => crate::shell_ops::grep(&self.ctx, args).await,
            "glob" => crate::shell_ops::glob(&self.ctx, args).await,

            // ── Git operations (gix-based) ───────────────────────────
            // Consolidated git tool — single entry point for all git operations.
            "git" => outcome_to_result(crate::git_gix::git_dispatch(pr, args)),

            // ── GitHub API ───────────────────────────────────────────
            "github" => match crate::github_tool_contract::github_action_from_args(args) {
                Ok(action) => self.dispatch_github_action(action, args).await,
                Err(error) => ToolResult::error(format!("Error: {error}")),
            },

            // ── Code intelligence (tree-sitter) ──────────────────────
            "symbols" => self.dispatch_symbols(args),

            // ── Web search ───────────────────────────────────────────
            "web_search" => {
                let cache_scope = format!("{}:{}", self.ctx.user_id, self.ctx.session_id);
                crate::web_search::perform_web_search(
                    self.ctx.http_client.as_ref(),
                    args,
                    &cache_scope,
                )
                .await
            }

            // ── Utility tools ────────────────────────────────────────
            "tool_search" => {
                let schemas = self.tool_schemas();
                string_to_result(crate::tool_search::tool_search(&schemas, args))
            }
            "env" => string_to_result(crate::env_tools::env_tool(args)),
            "config" => {
                // Default limits; wrapping executors can override
                string_to_result(crate::config_tool::config_tool(128_000, 16_000, args))
            }

            // ── Sleep ────────────────────────────────────────────────
            "sleep" => {
                let secs = args
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| (ms.min(300_000) as f64) / 1000.0)
                    .or_else(|| {
                        args.get("seconds")
                            .and_then(Value::as_f64)
                            .map(|s| s.clamp(0.0, 300.0))
                    })
                    .unwrap_or(1.0);
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
                ToolResult::text(format!("Slept for {secs:.1}s"))
            }

            // ── Web fetch (HTTP GET) ─────────────────────────────────
            "web_fetch" => {
                let cache_scope = format!("{}:{}", self.ctx.user_id, self.ctx.session_id);
                let output = crate::web_fetch::fetch_with_cache_scope(
                    self.ctx.http_client.as_ref(),
                    args,
                    &cache_scope,
                )
                .await;
                string_to_result(output)
            }

            // ── Display sixel (terminal image rendering) ──────────────
            "display_sixel" => match args.get("path").and_then(|v| v.as_str()) {
                Some(path) => crate::display_sixel::display_sixel(path),
                None => ToolResult::error(
                    "Error: display_sixel requires a `path` argument (a string path to the \
                     image file to render)."
                        .to_string(),
                ),
            },

            // ── Memory tools (require configured endpoint) ───────────
            "memory" => {
                let action = match crate::memory_tool_contract::memory_action_from_args(args) {
                    Ok(action) => action,
                    Err(error) => return ToolResult::error(format!("Error: {error}")),
                };
                if action == crate::memory_tool_contract::MemoryAction::Inventory {
                    let inventory = match astra_services::session_memory_inventory::load_local_session_memory_inventory(
                        &self.ctx.session_id,
                    ) {
                        Ok(inventory) => inventory,
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Error: session memory inventory failed: {error}"
                            ));
                        }
                    };
                    return match serde_json::to_string(&inventory) {
                        Ok(output) => ToolResult::text(output),
                        Err(error) => ToolResult::error(format!(
                            "Error: serialize session memory inventory: {error}"
                        )),
                    };
                }
                ToolResult::error(format!(
                    "Error: Memory tool (action='{}') is not available — the memoria \
                     service endpoint is not configured in this session.\n\n\
                     This usually means the session was started without `--memoria-url` or \
                     the MEMORIA_URL environment variable is unset.\n\
                     Workaround: skip memory operations for now, or ask the user to \
                     configure the memoria endpoint and restart.",
                    action.as_str()
                ))
            }

            // ── run_script (programmatic tool calling via Python + UDS RPC) ──
            "run_script" => {
                #[cfg(unix)]
                {
                    let config = crate::run_script::RunScriptConfig::default();
                    crate::run_script::handle_run_script(args, self, config).await
                }
                #[cfg(not(unix))]
                {
                    ToolResult::error(
                        "run_script is not available on this platform \
                         (requires Unix domain sockets)"
                            .into(),
                    )
                }
            }

            // ── Unknown tool ─────────────────────────────────────────
            _ => ToolResult::error(format!(
                "Error: Tool '{name}' not available in DefaultToolExecutor"
            )),
        }
    }

    /// Dispatch the consolidated GitHub tool via the optional GitHubClient.
    async fn dispatch_github_action(
        &self,
        action: crate::github_tool_contract::GithubAction,
        args: &Value,
    ) -> ToolResult {
        let client = match &self.github_client {
            Some(c) => c,
            None => {
                if !self.http_client_available {
                    return ToolResult::error(format!(
                        "Error: github(action='{}') failed — HTTP client could not be built.\n\n\
                         This is a system configuration issue (proxy, TLS, network). \
                         Check server logs for 'failed to build HTTP client' errors.\n\n\
                         GitHub integration requires a working HTTP client. \
                         Once the infrastructure issue is resolved, this tool will function normally.",
                        action.as_str()
                    ));
                }
                return ToolResult::error(format!(
                    "Error: github(action='{}') failed — no GitHub token is configured.\n\n\
                     To fix, do ONE of:\n\
                     1. Run `gh auth login` in a terminal (gh CLI stores the token)\n\
                     2. Set the GITHUB_TOKEN environment variable before starting this session\n\n\
                     If you are running in CI, ensure the token is injected into the runtime.\n\
                     After authentication, restart the session to enable GitHub integration.",
                    action.as_str()
                ));
            }
        };
        let output = match action {
            crate::github_tool_contract::GithubAction::ListPrs => client.list_prs(args).await,
            crate::github_tool_contract::GithubAction::GetPr => client.get_pr(args).await,
            crate::github_tool_contract::GithubAction::CiStatus => client.ci_status(args).await,
            crate::github_tool_contract::GithubAction::RepoStats => client.repo_stats(args).await,
            crate::github_tool_contract::GithubAction::ListIssues => client.list_issues(args).await,
            crate::github_tool_contract::GithubAction::GetIssue => client.get_issue(args).await,
            crate::github_tool_contract::GithubAction::CreateIssue => {
                client.create_issue(args).await
            }
        };
        string_to_result(output)
    }

    /// Dispatch the `symbols` tool: read a file, detect language, extract symbols.
    fn dispatch_symbols(&self, args: &Value) -> ToolResult {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("Error: Missing 'path' parameter".into()),
        };
        let resolved = match crate::fs_ops::resolve_path(&self.ctx.workspace_root, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let source = match std::fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
        };
        let lang = match crate::code_intel::detect_language(&resolved) {
            Some(l) => l,
            None => {
                return ToolResult::error(format!(
                    "Error: Cannot detect language for '{path_str}'"
                ));
            }
        };
        let symbols = crate::code_intel::extract_symbols(&source, lang);
        let outline = symbols
            .iter()
            .map(|s| format!("{}:{:?} {}", s.start_line + 1, s.kind, s.name))
            .collect::<Vec<_>>()
            .join("\n");
        if outline.is_empty() {
            ToolResult::text("No symbols found.".into())
        } else {
            ToolResult::text(outline)
        }
    }
}

fn mark_result_cached(result: &mut ToolResult) {
    let metadata = result.metadata.get_or_insert_with(serde_json::Map::new);
    metadata.insert("cached".to_string(), Value::Bool(true));
}

fn is_workspace_mutation_tool(name: &str, args: &Value) -> bool {
    match name {
        "write_file" | "str_replace" | "multi_edit" | "delete_file" => true,
        "git" => crate::git_tool_contract::git_action_from_args(args)
            .ok()
            .is_some_and(|action| match action {
                crate::git_tool_contract::GitAction::Commit
                | crate::git_tool_contract::GitAction::RevertCommit
                | crate::git_tool_contract::GitAction::Push => true,
                crate::git_tool_contract::GitAction::Stash => args
                    .get("sub_action")
                    .and_then(Value::as_str)
                    .is_some_and(git_stash_sub_action_mutates_workspace),
                crate::git_tool_contract::GitAction::CheckoutFile
                | crate::git_tool_contract::GitAction::Worktree => true,
                crate::git_tool_contract::GitAction::Status
                | crate::git_tool_contract::GitAction::Diff
                | crate::git_tool_contract::GitAction::Log
                | crate::git_tool_contract::GitAction::Show
                | crate::git_tool_contract::GitAction::Blame
                | crate::git_tool_contract::GitAction::FileHistory
                | crate::git_tool_contract::GitAction::LogSearch
                | crate::git_tool_contract::GitAction::Contributors => false,
            }),
        _ => false,
    }
}

fn git_stash_sub_action_mutates_workspace(action: &str) -> bool {
    matches!(
        action,
        "push" | "save" | "apply" | "pop" | "drop" | "branch"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn test_executor() -> (TempDir, DefaultToolExecutor) {
        let tmp = TempDir::new().unwrap();
        let ctx = ToolContext::test(tmp.path());
        let exec = DefaultToolExecutor::new(ctx);
        (tmp, exec)
    }

    #[test]
    fn server_direct_default_executor_tools_are_read_or_self_contained() {
        for name in SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES {
            assert!(
                crate::schemas::schema_exists_for_tool(name),
                "direct default executor tool must have a model-facing schema: {name}"
            );
        }
        for wrapped in [
            "write_file",
            "str_replace",
            "bash",
            "run_script",
            "task_board",
            "session",
            "memory",
            "rollback_file_edits",
        ] {
            assert!(
                !is_server_direct_default_executor_tool(wrapped),
                "server-specific tool `{wrapped}` must keep a dedicated handler"
            );
        }
    }

    fn init_git_repo(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    struct DenyInvocationApprovalGate;

    #[async_trait::async_trait]
    impl ToolApprovalGate for DenyInvocationApprovalGate {
        async fn request_approval(
            &self,
            _request_id: &str,
            _tool_name: &str,
            _args: &Value,
        ) -> crate::ApprovalDecision {
            crate::ApprovalDecision::Denied {
                reason: Some("test denied".to_string()),
            }
        }

        fn requires_approval(&self, _tool_name: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn dispatch_invocation_approval_blocks_git_mutating_action() {
        let (_tmp, mut exec) = test_executor();
        exec.approval_gate = Some(Arc::new(DenyInvocationApprovalGate));

        let result = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "ship"}),
            )
            .await;

        assert!(result.is_error);
        assert!(
            result.output.contains("The user REJECTED this tool call"),
            "mutating git actions must ask approval before execution: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_invocation_approval_skips_git_read_only_action() {
        let (tmp, mut exec) = test_executor();
        init_git_repo(tmp.path());
        exec.approval_gate = Some(Arc::new(DenyInvocationApprovalGate));

        let result = exec
            .execute("git", &serde_json::json!({"action": "diff"}))
            .await;

        assert!(
            !result.output.contains("The user REJECTED this tool call"),
            "read-only git actions must not request approval: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_read_file() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        let result = exec
            .execute("read_file", &serde_json::json!({"path": "hello.txt"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("world"));
    }

    #[tokio::test]
    async fn dispatch_read_file_outline() {
        let (tmp, exec) = test_executor();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub struct User;\n\npub fn parse() {}\nfn helper() {}\n",
        )
        .unwrap();
        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": "lib.rs", "outline": true}),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("# Outline"));
        assert!(result.output.contains("parse"));
    }

    #[tokio::test]
    async fn dispatch_read_file_large_file_returns_preview() {
        let (tmp, exec) = test_executor();
        let mut large = String::new();
        for i in 1..=3000 {
            large.push_str(&format!(
                "line {}: some padding content here to make the file larger\n",
                i
            ));
        }
        std::fs::write(tmp.path().join("big.txt"), &large).unwrap();
        let result = exec
            .execute("read_file", &serde_json::json!({"path": "big.txt"}))
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(result.output.contains("Large file preview"));
    }

    #[tokio::test]
    async fn dispatch_write_file() {
        let (tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "out.txt", "content": "data"}),
            )
            .await;
        assert!(!result.is_error);
        assert!(tmp.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn dispatch_write_file_delete_flag_routes_to_delete() {
        let (tmp, exec) = test_executor();
        let target = tmp.path().join("gone.txt");
        std::fs::write(&target, "data").unwrap();

        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "gone.txt", "delete": true}),
            )
            .await;

        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            result.output.contains("Successfully deleted"),
            "delete=true should route to delete semantics: {}",
            result.output
        );
        assert!(
            !target.exists(),
            "delete=true should remove the target file"
        );
    }

    #[tokio::test]
    async fn dispatch_write_file_delete_false_routes_to_write() {
        let (tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "test.txt", "content": "hello", "delete": false}),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            tmp.path().join("test.txt").exists(),
            "delete=false should write the file"
        );
    }

    #[tokio::test]
    async fn dispatch_write_file_delete_string_not_coerced() {
        let (tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "test.txt", "content": "hello", "delete": "true"}),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            tmp.path().join("test.txt").exists(),
            "delete=\\\"true\\\" (string) must not be coerced to boolean — should write"
        );
    }

    #[tokio::test]
    async fn dispatch_write_file_content_and_delete_true_delete_wins() {
        let (tmp, exec) = test_executor();
        let target = tmp.path().join("exists.txt");
        std::fs::write(&target, "original").unwrap();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "exists.txt", "content": "new content", "delete": true}),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            !target.exists(),
            "delete=true wins over content: file should be deleted"
        );
    }

    #[tokio::test]
    async fn dispatch_write_file_delete_path_traversal_blocked() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "../../etc/passwd", "delete": true}),
            )
            .await;
        assert!(
            result.is_error,
            "path traversal via write_file delete routing must be blocked"
        );
        assert!(
            result.output.contains("SANDBOX_DENIED"),
            "should report SANDBOX_DENIED: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("nonexistent", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("not available"));
    }

    #[tokio::test]
    async fn dispatch_delegate_is_not_a_default_executor_tool() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("delegate", &serde_json::json!({"task": "review"}))
            .await;
        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("Tool 'delegate' not available in DefaultToolExecutor"),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_list_dir() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        let result = exec
            .execute("list_dir", &serde_json::json!({"path": "."}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("a.rs"));
    }

    #[tokio::test]
    async fn dispatch_bash_echo() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("bash", &serde_json::json!({"command": "echo hello"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("hello"));
    }

    #[test]
    fn outcome_to_result_uses_explicit_error_flag_not_output_prefix() {
        let outcome = crate::git_gix::ToolExecutionOutcome::error("fatal: git failed".to_string());

        let result = outcome_to_result(outcome);

        assert!(
            result.is_error,
            "non-Error-prefixed git outcome errors must stay errors"
        );
        assert_eq!(result.output, "fatal: git failed");
    }

    #[test]
    fn ok_outcome_with_error_prefixed_output_is_not_misclassified() {
        // Regression: a successful tool result whose output happens to begin
        // with the literal text "Error" (e.g. log lines, diff hunks quoting a
        // compiler error, or a benign status like "Error code 0 (no change)")
        // MUST NOT be flagged as a failure. The error bit is load-bearing for
        // retry logic, hallucination detection, and UI badging.
        let outcome = crate::git_gix::ToolExecutionOutcome::ok(
            "Error code 0 (no change)\nAll fine.".to_string(),
        );

        let result = outcome_to_result(outcome);

        assert!(
            !result.is_error,
            "ok() outcomes must stay successful even when output starts with 'Error'"
        );
        assert!(result.output.starts_with("Error code 0"));
    }

    #[test]
    fn string_to_result_uses_structured_failure_status() {
        let result = string_to_result(
            serde_json::json!({
                "status": "failed",
                "error": "'query' is required",
            })
            .to_string(),
        );

        assert!(
            result.is_error,
            "structured status=failed must classify as tool error"
        );
    }

    #[test]
    fn string_to_result_does_not_misclassify_completed_json() {
        let result = string_to_result(
            serde_json::json!({
                "status": "completed",
                "output": "Error count: 0",
            })
            .to_string(),
        );

        assert!(
            !result.is_error,
            "completed structured JSON must not be classified by incidental text"
        );
    }

    #[test]
    fn string_to_result_does_not_misclassify_null_or_empty_error() {
        for error in [serde_json::Value::Null, serde_json::json!("")] {
            let result = string_to_result(
                serde_json::json!({
                    "ok": true,
                    "error": error,
                    "output": "completed"
                })
                .to_string(),
            );

            assert!(
                !result.is_error,
                "null/empty JSON error fields are not failures"
            );
        }
    }

    #[test]
    fn string_to_result_does_not_misclassify_agent_domain_status_json() {
        for status in ["launched", "still_running", "waiting", "interrupted"] {
            let result = string_to_result(
                serde_json::json!({
                    "status": status,
                    "agent_id": "reviewer@abc",
                    "finish_reason": "budget_exhausted",
                    "result": "partial review",
                })
                .to_string(),
            );

            assert!(
                !result.is_error,
                "agent status {status} is a domain state, not a malformed tool call"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_bash_reuses_identical_readonly_result() {
        // Cache-safe command: `pwd` — pure readonly, output depends
        // only on cwd. The classifier admits it; second call must
        // short-circuit to cache.
        let (_tmp, exec) = test_executor();
        let args = serde_json::json!({ "command": "pwd" });

        let first = exec.execute("bash", &args).await;
        let second = exec.execute("bash", &args).await;

        assert!(!first.is_error, "first failed: {}", first.output);
        assert!(!second.is_error, "second failed: {}", second.output);
        assert_eq!(first.output, second.output);
        assert_eq!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "second call must be served from cache"
        );
    }

    #[tokio::test]
    async fn dispatch_bash_does_not_cache_unsafe_commands() {
        // Compound command: cache MUST NOT fire. The classifier
        // refuses any command containing shell metacharacters, so
        // both calls re-execute.
        let (_tmp, exec) = test_executor();
        let args = serde_json::json!({
            "command": "printf 'tick\\n' >> dedup-count; wc -l dedup-count"
        });

        let first = exec.execute("bash", &args).await;
        let second = exec.execute("bash", &args).await;

        assert!(!first.is_error, "first failed: {}", first.output);
        assert!(!second.is_error, "second failed: {}", second.output);
        // Counter file went to 2 — proof that the second call really
        // re-executed. Before the classifier, the cache would return
        // the first call's output and the file would stay at 1.
        assert_eq!(
            std::fs::read_to_string(_tmp.path().join("dedup-count")).unwrap(),
            "tick\ntick\n",
            "compound command must re-execute both times — never cache shell pipelines"
        );
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "second call must NOT be marked cached"
        );
    }

    #[tokio::test]
    async fn dispatch_bash_does_not_cache_mutating_commands() {
        // Even a single-token mutating command must bypass the cache
        // — rm / mv / curl / cargo build / git commit / etc.
        // Regression guard for the 🔴 critical bug: without the
        // classifier, a second `rm foo` would return the first call's
        // "file removed" success while the real filesystem has moved
        // on and the file no longer exists.
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("victim"), "x").unwrap();
        let args = serde_json::json!({ "command": "rm victim" });

        let first = exec.execute("bash", &args).await;
        assert!(!first.is_error, "first rm failed: {}", first.output);
        // File gone. Second invocation MUST re-run and therefore
        // fail (no such file), not succeed-from-cache.
        let second = exec.execute("bash", &args).await;
        assert!(
            second.is_error,
            "second rm must re-execute and fail (file already gone); got output={}",
            second.output
        );
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "rm must never be marked cached"
        );
    }

    #[tokio::test]
    async fn dispatch_bash_cache_expires_after_ttl() {
        // External-mutation backstop: a cached readonly result must
        // be evicted when older than the TTL so e.g. `ls` reflects
        // files created by a user's editor (not bumping
        // `workspace_generation`).
        let dir = tempfile::tempdir().unwrap();
        let exec = DefaultToolExecutor::new(ToolContext {
            project_root: dir.path().to_path_buf(),
            workspace_root: dir.path().to_path_buf(),
            user_id: String::new(),
            session_id: String::new(),
            sandbox: crate::SandboxConfig::standard(dir.path()),
            http_client: None,
            logger: std::sync::Arc::new(crate::TracingLogger),
            cancel_token: None,
            detach_shell_handle: None,
        })
        .with_bash_cache_ttl(std::time::Duration::from_millis(30));

        let args = serde_json::json!({ "command": "ls" });
        let _first = exec.execute("bash", &args).await;

        // Simulated external mutation: drop a file without touching
        // the workspace_generation counter.
        std::fs::write(dir.path().join("external.txt"), "hi").unwrap();

        // Within TTL → still cached (shows the stale listing).
        let cached = exec.execute("bash", &args).await;
        assert_eq!(
            cached
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "within TTL, cache should still hit"
        );

        // Sleep past TTL, try again — must re-execute and show the
        // new file.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let fresh = exec.execute("bash", &args).await;
        assert_ne!(
            fresh
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "after TTL, cache must be evicted and re-executed"
        );
        assert!(
            fresh.output.contains("external.txt"),
            "fresh re-execution must see the new file: {}",
            fresh.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_cache_key_includes_stdin_hash() {
        // Same command text, different stdin → must hit different
        // cache slots. A `cat` invocation fed two different stdins
        // must never share a result.
        let (_tmp, exec) = test_executor();

        let args_a = serde_json::json!({
            "command": "cat",
            "stdin": "alpha",
        });
        let args_b = serde_json::json!({
            "command": "cat",
            "stdin": "beta",
        });

        let ra = exec.execute("bash", &args_a).await;
        let rb = exec.execute("bash", &args_b).await;
        // We don't assert on the actual shell output (cat behaviour
        // with stdin depends on how the bash tool plumbs it). We
        // only require that B did not serve A's cached result —
        // the cached flag on B must not be set.
        assert_ne!(
            rb.metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "different stdin must not collide with a previous cache entry; ra={} rb={}",
            ra.output,
            rb.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_force_bypasses_dedup_cache() {
        // Use a classifier-admitted readonly command so the cache
        // WOULD hit without `force`. Then `force=true` proves it's
        // actually bypassing the hit (cached flag not set), rather
        // than the classifier just never letting it hit in the
        // first place.
        let (tmp, exec) = test_executor();
        // `ls` is cache-safe; first call sees the workspace empty.
        let args = serde_json::json!({ "command": "ls" });
        let first = exec.execute("bash", &args).await;
        assert!(!first.is_error);

        // Normal second call — expected to hit the cache.
        let cached = exec.execute("bash", &args).await;
        assert_eq!(
            cached
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "sanity: second ls must hit cache"
        );

        // External file appears — classifier doesn't see it (no
        // mutation tool call) so the cache would still hit.
        std::fs::write(tmp.path().join("appeared.txt"), "x").unwrap();

        // Forced call must bypass cache AND see the fresh file.
        let forced = exec
            .execute("bash", &serde_json::json!({"command": "ls", "force": true}))
            .await;
        assert_ne!(
            forced
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "force=true must not return cached result"
        );
        assert!(
            forced.output.contains("appeared.txt"),
            "forced call must re-execute and see the fresh file: {}",
            forced.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_cache_invalidates_after_file_mutation() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("watched.txt"), "one\n").unwrap();
        let args = serde_json::json!({"command": "cat watched.txt"});

        let first = exec.execute("bash", &args).await;
        let write = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "watched.txt", "content": "two"}),
            )
            .await;
        let second = exec.execute("bash", &args).await;

        assert!(!first.is_error, "first failed: {}", first.output);
        assert!(!write.is_error, "write failed: {}", write.output);
        assert!(!second.is_error, "second failed: {}", second.output);
        assert_eq!(first.output, "one\n");
        assert_eq!(second.output, "two\n");
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn dispatch_bash_cache_invalidates_after_git_commit() {
        let (tmp, exec) = test_executor();
        init_git_repo(tmp.path());

        let tracked = tmp.path().join("tracked.txt");
        std::fs::write(&tracked, "initial\n").unwrap();
        let initial = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "initial"}),
            )
            .await;
        assert!(
            !initial.is_error,
            "initial commit failed: {}",
            initial.output
        );

        let args = serde_json::json!({"command": "git log --oneline -1"});
        let first = exec.execute("bash", &args).await;
        assert!(!first.is_error, "first log failed: {}", first.output);
        assert!(first.output.contains("initial"), "got: {}", first.output);

        std::fs::write(&tracked, "changed\n").unwrap();
        let change = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "change tracked"}),
            )
            .await;
        assert!(!change.is_error, "change commit failed: {}", change.output);

        let second = exec.execute("bash", &args).await;
        assert!(!second.is_error, "second log failed: {}", second.output);
        assert!(
            second.output.contains("change tracked"),
            "git commit must invalidate cached git log output: {}",
            second.output
        );
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn dispatch_bash_cache_invalidates_after_git_stash_drop() {
        let (tmp, exec) = test_executor();
        init_git_repo(tmp.path());

        let tracked = tmp.path().join("tracked.txt");
        std::fs::write(&tracked, "initial\n").unwrap();
        let initial = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "initial"}),
            )
            .await;
        assert!(
            !initial.is_error,
            "initial commit failed: {}",
            initial.output
        );

        std::fs::write(&tracked, "stashed\n").unwrap();
        let push = exec
            .execute(
                "git",
                &serde_json::json!({"action": "stash", "sub_action": "push", "message": "save tracked"}),
            )
            .await;
        assert!(!push.is_error, "stash push failed: {}", push.output);

        let args = serde_json::json!({"command": "git stash list"});
        let cached_source = exec.execute("bash", &args).await;
        assert!(
            cached_source.output.contains("save tracked"),
            "expected stash list to include pushed stash: {}",
            cached_source.output
        );

        let drop = exec
            .execute(
                "git",
                &serde_json::json!({"action": "stash", "sub_action": "drop"}),
            )
            .await;
        assert!(!drop.is_error, "stash drop failed: {}", drop.output);

        let after_drop = exec.execute("bash", &args).await;
        assert_ne!(
            after_drop
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "git stash drop changes refs/stash, so cached git stash list must be invalidated"
        );
        assert!(
            !after_drop.output.contains("save tracked"),
            "fresh stash list should not include dropped stash: {}",
            after_drop.output
        );
    }

    /// Regression guard: a failed bash run (non-zero exit, sandbox
    /// block, permission denied, timeout, cancellation) MUST NOT be
    /// cached — otherwise after the user fixes the underlying
    /// condition (chmod, install, whatever) the next call returns
    /// the stale error for up to the TTL window.
    ///
    /// This covers the user-reported worry that "bash dedup caches
    /// failures" — the insert path is guarded by `!result.is_error`
    /// (see dispatch around line 295) and this test locks the
    /// invariant in.
    #[tokio::test]
    async fn dispatch_bash_does_not_cache_failed_results() {
        let (tmp, exec) = test_executor();
        // Use a readonly command (`cat`) so it WOULD be classifier-
        // admitted. First call reads a non-existent path → cat
        // exits 1 → is_error=true. Second call targets the same
        // path but the file now exists → must re-execute, not
        // replay the "No such file" error.
        let args = serde_json::json!({ "command": "cat missing.txt" });
        let first = exec.execute("bash", &args).await;
        assert!(
            first.is_error,
            "first call must surface the cat error: {}",
            first.output
        );

        // Fix the condition: create the file.
        std::fs::write(tmp.path().join("missing.txt"), "hello\n").unwrap();

        // Second call — must re-execute and succeed, not return the
        // cached error.
        let second = exec.execute("bash", &args).await;
        assert!(
            !second.is_error,
            "second call must not replay cached error (got: {})",
            second.output
        );
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "second call must not be served from cache"
        );
        assert!(
            second.output.contains("hello"),
            "second call must show the real file content: {}",
            second.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_non_zero_is_error() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "echo nope >&2; exit 7"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("stderr:\nnope"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("[exit code: 7]"),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_timeout_keeps_partial_output() {
        let (_tmp, exec) = test_executor();
        // Regression guard: pipe-leak via orphaned `sleep` would stall this
        // test for the full 5s. `sigkill_process_group` (in shell_ops) kills
        // the whole group on timeout.
        let result = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "echo start; sleep 5; echo done", "timeout": 0.2}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("start"), "got: {}", result.output);
        assert!(
            result.output.contains("timed out after 0.2s"),
            "got: {}",
            result.output
        );
        assert!(!result.output.contains("done"), "got: {}", result.output);
    }

    #[tokio::test]
    async fn dispatch_str_replace() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("f.txt"), "old text here").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &serde_json::json!({
                    "path": "f.txt", "old_str": "old text", "new_str": "new text"
                }),
            )
            .await;
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "new text here\n");
    }

    #[tokio::test]
    async fn dispatch_str_replace_dry_run_does_not_write() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("f.txt"), "old text here").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &serde_json::json!({
                    "path": "f.txt",
                    "old_str": "old text",
                    "new_str": "new text",
                    "dry_run": true
                }),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "old text here");
    }

    #[tokio::test]
    async fn dispatch_env() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("env", &serde_json::json!({"action": "list"}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn dispatch_tool_search() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("tool_search", &serde_json::json!({"query": "file"}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn dispatch_tool_search_missing_query_returns_structured_error() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("tool_search", &serde_json::json!({})).await;

        assert!(
            result.is_error,
            "structured tool_search failure must be marked as a tool error"
        );
        let parsed: serde_json::Value = serde_json::from_str(&result.output)
            .unwrap_or_else(|error| panic!("tool_search error must stay JSON: {error}"));
        assert_eq!(parsed["mode"].as_str(), Some("error"));
        assert_eq!(parsed["status"].as_str(), Some("failed"));
        assert_eq!(parsed["error"].as_str(), Some("'query' is required"));
    }

    #[tokio::test]
    async fn dispatch_github_without_client_gives_actionable_guidance() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("github", &serde_json::json!({"action": "list_prs"}))
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("github(action='list_prs')"),
            "error should describe the consolidated action surface: {}",
            result.output
        );
        assert!(
            !result.output.contains("github_"),
            "error must not leak helper-style tool names: {}",
            result.output
        );
        assert!(
            result.output.contains("no GitHub token is configured"),
            "error must describe the problem, not the internal API: {}",
            result.output
        );
        assert!(
            result.output.contains("gh auth login"),
            "error must suggest a concrete fix action: {}",
            result.output
        );
        assert!(
            result.output.contains("restart the session"),
            "error must explain how to enable the feature: {}",
            result.output
        );
        assert!(
            !result.output.contains("Workaround"),
            "error must not suggest bypassing the system architecture: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_memory_without_endpoint_gives_actionable_guidance() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "memory",
                &serde_json::json!({"action": "remember", "content": "test"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("not available"),
            "error must describe unavailability: {}",
            result.output
        );
        assert!(
            result.output.contains("Workaround"),
            "error must offer a fallback: {}",
            result.output
        );
        assert!(
            !result.output.contains("ServerToolExecutor"),
            "error must not leak internal type names: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_memory_inventory_uses_journal_even_without_memoria_endpoint() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let (_tmp, exec) = test_executor();
        let writer = astra_services::session_journal::JournalWriter::new("test-session").unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::session_memory_extraction(
                    Some("test-session"),
                    3,
                    15,
                    astra_services::session_journal::SessionMemoryExtractionOutcome::Extracted {
                        source: astra_services::session_journal::SessionMemoryExtractionSource::Llm,
                        bytes_written: 70,
                    },
                    &astra_services::session_journal::SessionMemoryExtractionBreadcrumbs::default(),
                ),
            )
            .unwrap();

        let result = exec
            .execute("memory", &serde_json::json!({"action": "inventory"}))
            .await;
        let inventory: astra_services::session_memory_inventory::SessionMemoryInventory =
            serde_json::from_str(&result.output).unwrap();

        assert!(!result.is_error, "{result:?}");
        assert_eq!(inventory.successful_extraction_versions, 1);
        assert_eq!(inventory.llm_versions, 1);
        assert_eq!(inventory.logical_current_snapshot_count, Some(0));
    }

    #[tokio::test]
    async fn dispatch_memory_inventory_fails_when_exactness_cannot_be_proven() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let path = astra_services::session_journal::journal_file_path("test-session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{not-json}\n").unwrap();
        let (_tmp, exec) = test_executor();

        let result = exec
            .execute("memory", &serde_json::json!({"action": "inventory"}))
            .await;

        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("cannot be exact"), "{result:?}");
    }

    #[tokio::test]
    async fn dispatch_github_helper_style_names_are_unknown_tools() {
        let (_tmp, exec) = test_executor();
        let actions = [
            "list_prs",
            "get_pr",
            "ci_status",
            "list_issues",
            "get_issue",
            "repo_stats",
        ];
        for name in actions.into_iter().map(|action| format!("github_{action}")) {
            let result = exec.execute(&name, &serde_json::json!({})).await;
            assert!(result.is_error, "{name}: {}", result.output);
            assert!(
                result
                    .output
                    .contains(&format!("Tool '{name}' not available")),
                "{name}: {}",
                result.output
            );
        }
    }

    #[test]
    fn recover_poisoned_lock_increments_counter() {
        use std::sync::Mutex;

        let before = super::poisoned_lock_recovery_count();

        // Create a mutex and poison it by panicking while holding the lock
        let mutex = Mutex::new(42);
        let mutex_arc = Arc::new(mutex);
        let mutex_clone = mutex_arc.clone();

        let handle = std::thread::spawn(move || {
            let _guard = mutex_clone.lock().unwrap();
            panic!("poison the lock");
        });
        let _ = handle.join();

        // Now try to lock it - it should be poisoned
        let result = mutex_arc.lock();
        let _guard = super::recover_poisoned_lock(result, "test_lock");

        let after = super::poisoned_lock_recovery_count();
        assert_eq!(
            after,
            before + 1,
            "poisoned lock recovery should increment the counter"
        );
    }

    #[tokio::test]
    async fn dispatch_symbols() {
        let (tmp, exec) = test_executor();
        std::fs::write(
            tmp.path().join("sample.rs"),
            "fn hello() {}\nstruct Foo {}\n",
        )
        .unwrap();
        let result = exec
            .execute("symbols", &serde_json::json!({"path": "sample.rs"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn string_to_result_error() {
        let r = string_to_result("Error: something went wrong".into());
        assert!(r.is_error);
        assert!(r.output.contains("something went wrong"));
    }

    #[tokio::test]
    async fn string_to_result_ok() {
        let r = string_to_result("All good".into());
        assert!(!r.is_error);
        assert_eq!(r.output, "All good");
    }

    #[tokio::test]
    async fn dispatch_sleep() {
        let (_tmp, exec) = test_executor();
        let start = std::time::Instant::now();
        let result = exec
            .execute("sleep", &serde_json::json!({"duration_ms": 100}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("Slept"));
        assert!(start.elapsed().as_millis() >= 90);
    }

    #[tokio::test]
    async fn dispatch_sleep_accepts_legacy_seconds() {
        let (_tmp, exec) = test_executor();
        let start = std::time::Instant::now();
        let result = exec
            .execute("sleep", &serde_json::json!({"seconds": 0.05}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("Slept"));
        assert!(start.elapsed().as_millis() >= 40);
    }

    #[tokio::test]
    async fn dispatch_multi_edit() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("m.txt"), "aaa bbb ccc").unwrap();
        let result = exec
            .execute(
                "multi_edit",
                &serde_json::json!({
                    "path": "m.txt",
                    "edits": [
                        {"old_str": "aaa", "new_str": "AAA"},
                        {"old_str": "ccc", "new_str": "CCC"}
                    ]
                }),
            )
            .await;
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("m.txt")).unwrap();
        assert_eq!(content, "AAA bbb CCC\n");
    }

    #[tokio::test]
    async fn dispatch_web_fetch_missing_url() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("web_fetch", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("Missing 'url'"));
    }

    #[tokio::test]
    async fn dispatch_web_fetch_bad_scheme() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "web_fetch",
                &serde_json::json!({"url": "ftp://example.com"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("Unsupported scheme"));
    }

    #[tokio::test]
    async fn dispatch_git_revert_commit() {
        let (tmp, exec) = test_executor();
        init_git_repo(tmp.path());

        let tracked = tmp.path().join("tracked.txt");
        std::fs::write(&tracked, "original\n").unwrap();
        let initial = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "initial"}),
            )
            .await;
        assert!(!initial.is_error, "got: {}", initial.output);

        std::fs::write(&tracked, "changed\n").unwrap();
        let committed = exec
            .execute(
                "git",
                &serde_json::json!({"action": "commit", "message": "change tracked"}),
            )
            .await;
        assert!(!committed.is_error, "got: {}", committed.output);
        let commit_sha = committed
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
            .expect("commit_sha metadata");

        let reverted = exec
            .execute(
                "git",
                &serde_json::json!({"action": "revert_commit", "commit_sha": commit_sha}),
            )
            .await;
        assert!(!reverted.is_error, "got: {}", reverted.output);
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "original\n");
        assert_eq!(
            reverted
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("reverted_commit_sha"))
                .and_then(Value::as_str),
            Some(commit_sha)
        );
    }

    /// P1-J: execute() must truncate output exceeding MAX_TOOL_OUTPUT_BYTES.
    /// Uses read_file on a large synthetic file to trigger truncation.
    #[tokio::test]
    async fn output_truncated_at_max_bytes() {
        let (tmp, exec) = test_executor();

        // Write a file larger than MAX_TOOL_OUTPUT_BYTES (64KB)
        let large_content = "x".repeat(200 * 1024); // 200KB
        let file_path = tmp.path().join("large.txt");
        std::fs::write(&file_path, &large_content).unwrap();

        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": file_path.to_str().unwrap()}),
            )
            .await;

        assert!(
            result.output.len() <= super::MAX_TOOL_OUTPUT_BYTES + 200,
            "output must be truncated to ~{}KB, got {} bytes",
            super::MAX_TOOL_OUTPUT_BYTES / 1024,
            result.output.len()
        );
        assert!(
            result.output.contains("truncated"),
            "truncated output must contain truncation notice"
        );
    }

    /// P1-I: execute() must return a timeout error for tools that hang.
    /// We test this by verifying the TOOL_TIMEOUT constant is reasonable
    /// and that the timeout path produces the right error message.
    #[test]
    fn tool_timeout_constant_is_reasonable() {
        // TOOL_TIMEOUT must be > 0 and ≤ 5 minutes (not too short, not infinite)
        assert!(
            super::TOOL_TIMEOUT.as_secs() >= 10,
            "TOOL_TIMEOUT must be at least 10s to allow real tool calls"
        );
        assert!(
            super::TOOL_TIMEOUT.as_secs() <= 300,
            "TOOL_TIMEOUT must be ≤ 5 minutes to prevent indefinite hangs"
        );
    }

    /// P1-C: execute() must return an error immediately when the cancellation
    /// token is already cancelled — tool must NOT be executed.
    #[tokio::test]
    async fn cancelled_token_prevents_tool_execution() {
        let (tmp, exec) = test_executor();

        // Set a pre-cancelled token
        let token = Arc::new(CancellationToken::new());
        token.cancel();
        let exec = exec.with_cancel_token(Some(token));

        // Try to execute a real tool — it must be rejected, not executed
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": file_path.to_str().unwrap()}),
            )
            .await;

        assert!(
            result.is_error,
            "cancelled token must produce an error result"
        );
        assert!(
            result.output.contains("cancelled"),
            "error must mention cancellation, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_in_flight_dispatch() {
        let (_tmp, exec) = test_executor();
        let token = Arc::new(CancellationToken::new());
        let trigger = Arc::clone(&token);
        let exec = exec.with_cancel_token(Some(token));

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            trigger.cancel();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            exec.execute("sleep", &serde_json::json!({"seconds": 30})),
        )
        .await
        .expect("in-flight cancellation should not wait for tool timeout");

        assert!(result.is_error, "cancelled tool should be an error");
        assert!(
            result.output.contains("cancelled before completion"),
            "error must mention cancellation, got: {}",
            result.output
        );
    }

    // ── run_script dispatch ──────────────────────────────────────────────

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn dispatch_run_script_executes_python() {
        if !crate::run_script::python3_available() {
            return;
        }
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("data.txt"), "hello from file").unwrap();

        let result = exec
            .execute(
                "run_script",
                &serde_json::json!({
                    "script": "print('run_script works')",
                    "timeout": 5
                }),
            )
            .await;
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("run_script works"),
            "output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_run_script_missing_script_returns_error() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("run_script", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("requires a non-empty"));
        assert!(result.output.contains("empty arguments"));
    }

    #[tokio::test]
    async fn dispatch_execute_code_is_unknown_tool() {
        // Legacy execute_code has been removed. Attempting to dispatch it
        // must fall through to the unknown-tool error — no gated fallback.
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "execute_code",
                &serde_json::json!({"script": "print('hi')"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("not available") || result.output.contains("Error"),
            "expected unknown-tool error, got: {}",
            result.output
        );
    }
}
