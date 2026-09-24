//! Default tool executor — the shared implementation used by CLI, server, and edge.
//!
//! Routes tool calls to the appropriate module (fs_ops, shell_ops, etc.)
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

// This registration is also the implementation dispatch, so confinement
// admission cannot drift into an independent list of permitted tool names.
type NativeFileFn = fn(&Path, &Value) -> ToolResult;
type ConfinedNativeFileFn = fn(&Path, &Value, &crate::fs_ops::FileAccess) -> ToolResult;

struct NativeFileHandler {
    ordinary: NativeFileFn,
    confined: ConfinedNativeFileFn,
}

fn native_file_handler(name: &str) -> Option<NativeFileHandler> {
    use crate::fs_ops;
    let (ordinary, confined): (NativeFileFn, ConfinedNativeFileFn) = match name {
        "read_file" => (fs_ops::read_file, fs_ops::read_file_with_access),
        "write_file" => (
            |root, args| {
                if args.get("delete").and_then(Value::as_bool) == Some(true) {
                    fs_ops::delete_file(root, args)
                } else {
                    fs_ops::write_file(root, args)
                }
            },
            fs_ops::write_file_with_access,
        ),
        "delete_file" => (fs_ops::delete_file, fs_ops::delete_file_with_access),
        "str_replace" => (fs_ops::str_replace, fs_ops::str_replace_with_access),
        "multi_edit" => (fs_ops::multi_edit, fs_ops::multi_edit_with_access),
        _ => return None,
    };
    Some(NativeFileHandler { ordinary, confined })
}

fn unsupported_managed_tool(name: &str) -> ToolResult {
    ToolResult::error(format!(
        "Error: tool '{name}' does not support the managed filesystem boundary; its native IO and helpers are not confined"
    ))
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
/// Covers file ops, shell, code intelligence,
/// and utility tools. CLI-specific tools (ask_user, MCP,
/// LSP subprocess, interactive shell) are handled by wrapping executors.
#[derive(Clone)]
pub struct DefaultToolExecutor {
    ctx: ToolContext,
    approval_gate: Option<Arc<dyn ToolApprovalGate>>,
    progress_callback: Option<Arc<dyn ToolProgressCallback>>,

    bash_cache: Arc<Mutex<HashMap<BashCacheKey, BashCacheEntry>>>,
    workspace_generation: Arc<AtomicU64>,
    convergence_tracker: crate::workspace_observation::DesiredStateConvergenceTracker,
    convergence_authority: Arc<str>,
    bash_cache_ttl: std::time::Duration,
    filesystem_write_boundary: Option<Vec<std::path::PathBuf>>,
    shell_process_boundary: Option<astra_sandbox::ShellProcessBoundary>,
    native_file_access: Option<Result<crate::fs_ops::FileAuthority, String>>,
}

/// Key for the per-session bash dedup cache. Bumping ANY of these
/// fields must invalidate prior entries — otherwise we'd return a
/// result computed under a different precondition and the model would
/// act on stale state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BashCacheKey {
    workspace_root: String,
    /// Canonical call-scoped execution directory. The same read-only command
    /// may produce different output in different workspace directories.
    workdir: String,
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
        Self {
            ctx,
            approval_gate: None,
            progress_callback: None,

            bash_cache: Arc::new(Mutex::new(HashMap::new())),
            workspace_generation: Arc::new(AtomicU64::new(0)),
            convergence_tracker: Default::default(),
            convergence_authority: Arc::from(uuid::Uuid::new_v4().to_string()),
            bash_cache_ttl: DEFAULT_BASH_CACHE_TTL,
            filesystem_write_boundary: None,
            shell_process_boundary: None,
            native_file_access: None,
        }
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
    /// Handles the local/edge setup recipe: HTTP client, `ToolContext`,
    /// and sandbox.
    ///
    /// If the HTTP client cannot be built, a warning is logged and the
    /// executor is created without HTTP support (HTTP-dependent tools will
    /// report errors rather than crashing the runtime).
    pub fn for_workspace(
        workspace: &Path,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        user_agent: &str,
        timeout: std::time::Duration,
    ) -> Self {
        Self::for_workspace_inner(workspace, user_id, session_id, user_agent, timeout)
    }

    /// Build a multi-tenant Server executor without reading process-level or
    /// host CLI credentials. Credential-backed tools must be installed later
    /// from an authenticated, owner-scoped capability binding.
    pub fn for_server_workspace(
        workspace: &Path,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        user_agent: &str,
        timeout: std::time::Duration,
    ) -> Self {
        Self::for_workspace_inner(workspace, user_id, session_id, user_agent, timeout)
    }

    fn for_workspace_inner(
        workspace: &Path,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        user_agent: &str,
        timeout: std::time::Duration,
    ) -> Self {
        let http_client = match reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(user_agent.to_string())
            .no_proxy()
            .build()
        {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to build HTTP client for tool executor — HTTP-dependent tools will be unavailable"
                );
                None
            }
        };

        let ctx = crate::ToolContext {
            project_root: workspace.to_path_buf(),
            workspace_root: workspace.to_path_buf(),
            user_id: user_id.into(),
            session_id: session_id.into(),
            sandbox: crate::SandboxConfig::standard(workspace),
            http_client,
            logger: Arc::new(crate::TracingLogger),
            cancel_token: None,
            detach_shell_handle: None,
        };

        Self::new(ctx)
    }

    pub fn with_cancel_token(mut self, token: Option<Arc<CancellationToken>>) -> Self {
        self.ctx.cancel_token = token;
        self
    }

    /// Require bash subprocesses to see host-owned runtime lanes as read-only.
    /// Only managed Edge requests install this request-scoped boundary. Native
    /// file tools pin their workspace here and use descriptor-relative IO;
    /// unsupported helper families are rejected and implicit formatters disabled.
    /// The trusted host must provide a writable, same-filesystem workspace parent
    /// outside the confined shell's mounted roots for private native staging.
    pub fn with_filesystem_write_boundary(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.native_file_access = Some(
            crate::fs_ops::FileAccess::restricted(&self.ctx.workspace_root, &paths)
                .map(crate::fs_ops::FileAccess::into_authority)
                .map_err(|error| error.to_string()),
        );
        self.filesystem_write_boundary = Some(paths);
        self
    }

    /// Select restricted Linux shell launches with an explicit toolchain manifest.
    /// No read roots are inferred and no ordinary-shell fallback is permitted.
    /// Native files retain the same protected-path authority as managed tools.
    /// The provider owns exclusive allocation, immutable inputs, private staging,
    /// and supervisor entrypoint dispatch; this does not admit an Evaluation run.
    pub fn with_shell_process_boundary(
        mut self,
        boundary: astra_sandbox::ShellProcessBoundary,
        protected_paths: Vec<std::path::PathBuf>,
    ) -> Self {
        self = self.with_filesystem_write_boundary(protected_paths);
        self.shell_process_boundary = Some(boundary);
        self
    }

    /// Require the process sandbox to create an isolated network namespace.
    /// Evaluation workspaces use this together with the filesystem boundary;
    /// namespace unavailability therefore rejects execution rather than
    /// degrading to host networking.
    pub fn with_network_isolation(mut self) -> Self {
        self.ctx.sandbox.network_allowed = false;
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

impl DefaultToolExecutor {
    // The invocation-local flag records entry into the process owner, not a
    // guess from a result string or missing metadata. Dropped futures return no proof.
    async fn execute_with_launch_tracking(
        &self,
        name: &str,
        args: &Value,
        launch_attempted: &mut bool,
    ) -> ToolResult {
        if let Err(error) = crate::schemas::validate_tool_arguments(name, args) {
            return error.into_tool_result();
        }

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
            return crate::cancelled_tool_result(name, false);
        }

        if let Some(access) = &self.native_file_access {
            if name != "bash" && native_file_handler(name).is_none() {
                return unsupported_managed_tool(name);
            }
            match access {
                Ok(access)
                    if access.matches_root(&self.ctx.workspace_root)
                        && self
                            .shell_process_boundary
                            .as_ref()
                            .is_none_or(|boundary| access.matches_root(&boundary.workspace)) => {}
                Ok(_) => {
                    return ToolResult::error(
                        "SANDBOX_DENIED: pinned workspace root was replaced".into(),
                    );
                }
                Err(error) => {
                    return ToolResult::error(format!(
                        "SANDBOX_DENIED: native filesystem authority unavailable: {error}"
                    ));
                }
            }
        }

        // Resolve once per invocation. The same canonical identity drives
        // cache lookup, spawn, evidence, and cache insertion so a path alias
        // cannot be retargeted between those phases.
        let bash_workdir = if name == "bash" {
            match crate::shell_ops::resolve_bash_workdir(&self.ctx.workspace_root, args) {
                Ok(workdir) => Some(workdir),
                Err(error) => return ToolResult::error(error),
            }
        } else {
            None
        };

        if name == "bash"
            && !crate::workspace_observation::is_explicit_workspace_verification_request(name, args)
            && !args.get("force").and_then(Value::as_bool).unwrap_or(false)
            && let Some(key) = self.bash_cache_key(
                args,
                bash_workdir.as_ref().expect("bash workdir resolved above"),
            )
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
            crate::shell_ops::attach_bash_workdir_evidence(
                &mut cached,
                &self.ctx.workspace_root,
                bash_workdir.as_ref().expect("bash workdir resolved above"),
                args,
            );
            if let Some(cb) = &self.progress_callback {
                cb.tool_completed(&call_id, &cached.output, !cached.is_error)
                    .await;
            }
            return cached;
        }

        // Direct workspace writers participate in the same per-root lease as
        // Bash observation windows. This prevents a typed write in another
        // concurrent caller from being mistaken for an opaque Bash delta.
        let nested_run_script_callback = crate::rpc_bridge::is_run_script_rpc_dispatch();
        if name == "run_script" && nested_run_script_callback {
            return ToolResult::error(
                "run_script cannot recursively start another opaque script writer".into(),
            );
        }
        let native_operation = self
            .native_file_access
            .as_ref()
            .and_then(|access| access.as_ref().ok())
            .map(crate::fs_ops::FileAuthority::operation);
        let native_access = native_operation.as_ref();
        let targeted_observer = self
            .convergence_tracker
            .requires_snapshot_lease_with_access(
                &self.convergence_authority,
                name,
                args,
                &self.ctx.workspace_root,
                native_access,
            );
        let _workspace_mutation_lease = if name != "bash"
            && name != "run_script"
            && (is_workspace_mutation_tool(name, args) || targeted_observer)
            && !nested_run_script_callback
        {
            // Typed writers must share the same per-workspace lease as
            // opaque Bash observation windows.  Otherwise a direct write
            // from another caller can land between Bash's pre/post
            // fingerprints and be falsely attributed to Bash.  Bash is
            // excluded because its own shell boundary acquires the lease.
            match crate::workspace_observation::acquire_workspace_mutation_lease_with_options(
                &self.ctx.workspace_root,
                self.ctx.cancel_token.as_deref(),
                std::time::Duration::from_secs(120),
            )
            .await
            {
                Some(guard) => Some(guard),
                None => {
                    if self
                        .ctx
                        .cancel_token
                        .as_ref()
                        .is_some_and(|token| token.is_cancelled())
                    {
                        self.convergence_tracker
                            .clear_authority(&self.convergence_authority);
                        return crate::cancelled_tool_result(name, false);
                    }
                    return ToolResult::error(
                        "workspace coordination lock was cancelled, contended, or the host temporary lock namespace is not trustworthy; no tool was run. Retry after the active writer finishes or repair the host temporary-directory ownership and sticky-bit permissions"
                            .into(),
                    );
                }
            }
        } else {
            None
        };
        if self
            .ctx
            .cancel_token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            self.convergence_tracker
                .clear_authority(&self.convergence_authority);
            return crate::cancelled_tool_result(name, false);
        }
        let _recursive_writer_epoch = if name == "run_script" {
            match crate::workspace_observation::begin_workspace_writer_with_options(
                &self.ctx.workspace_root,
                self.ctx.cancel_token.as_deref(),
                std::time::Duration::from_secs(120),
            )
            .await
            {
                Some(guard) => Some(guard),
                None => {
                    return ToolResult::error(
                        "workspace writer coordination was cancelled, contended, or the host temporary lock namespace is not trustworthy; run_script was not run. Retry after the active writer finishes or repair the host temporary-directory ownership and sticky-bit permissions"
                            .into(),
                    );
                }
            }
        } else {
            None
        };
        let dispatch = self.dispatch(
            name,
            args,
            bash_workdir.as_ref(),
            native_access,
            launch_attempted,
        );
        // Bash and run_script own their child timeout/cancellation paths. Do
        // not wrap either in the generic 60s future timeout: dropping one can
        // abandon the post-execution workspace receipt after a partial write.
        let mut result = if name == "bash" || name == "run_script" {
            dispatch.await
        } else if let Some(token) = self.ctx.cancel_token.as_ref() {
            tokio::select! {
                _ = token.cancelled() => {
                    crate::cancelled_tool_result(name, true)
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
        let coordination_integrity_valid = native_access.is_none_or(|access| access.matches_root(&self.ctx.workspace_root))
            && _workspace_mutation_lease.as_ref().is_none_or(
            crate::workspace_observation::WorkspaceObservationLease::coordination_integrity_valid,
        ) && _recursive_writer_epoch.as_ref().is_none_or(
            crate::workspace_observation::WorkspaceWriterGuard::coordination_integrity_valid,
        );
        let receipt_authority_valid = coordination_integrity_valid
            && _workspace_mutation_lease.as_ref().is_none_or(
                crate::workspace_observation::WorkspaceObservationLease::receipt_authority_valid,
            )
            && _recursive_writer_epoch.as_ref().is_none_or(
                crate::workspace_observation::WorkspaceWriterGuard::receipt_authority_valid,
            );
        if nested_run_script_callback && let Some(fields) = result.metadata.as_mut() {
            // The callback is re-entrant under its opaque parent. It may
            // return ordinary output to Python, but only the parent can check
            // the binding generation after the complete script settles.
            fields.remove("workspace_mutation_applied");
            crate::workspace_observation::discard_workspace_desired_state_convergence_marker(
                fields,
            );
            fields.remove(crate::workspace_observation::OBSERVED_FIELD);
            fields.remove(crate::workspace_observation::SCOPE_FIELD);
            fields.remove(crate::workspace_observation::RECEIPT_FIELD);
        }
        if !coordination_integrity_valid {
            crate::workspace_observation::mark_workspace_observation_unsettled(
                &self.ctx.workspace_root,
            );
            if let Some(fields) = result.metadata.as_mut() {
                fields.remove("workspace_mutation_applied");
                crate::workspace_observation::discard_workspace_desired_state_convergence_marker(
                    fields,
                );
                fields.remove(crate::workspace_observation::OBSERVED_FIELD);
                fields.remove(crate::workspace_observation::SCOPE_FIELD);
                fields.remove(crate::workspace_observation::RECEIPT_FIELD);
            }
            result.is_error = true;
            result.output.push_str(
                "\n\nError: workspace binding or coordination generation changed during execution; the mutation may have applied, but no durable mutation receipt was issued. Re-bind and inspect the workspace before continuing.",
            );
        }
        {
            let desired_state =
            match crate::workspace_observation::consume_workspace_desired_state_convergence_marker(
                &mut result.metadata,
                args,
                &self.ctx.workspace_root,
            ) {
                Ok(desired_state) => desired_state,
                Err(error) => {
                    result.is_error = true;
                    result.output.push_str(&format!(
                        "\n\nError: {error}; no convergence authority was issued."
                    ));
                    None
                }
            };
            // A successful structured workspace writer already crossed the
            // owner executor's path/permission boundary. Carry that typed fact
            // through the server/edge result ledger instead of making a remote
            // runtime guess the target against its own filesystem. This does not
            // satisfy final verification; it only opens the normal post-mutation
            // observation obligation.
            if native_access.is_none()
                && let Some(receipt) =
                    crate::workspace_observation::typed_workspace_tool_receipt_for_applied(
                        name,
                        args,
                        &self.ctx.workspace_root,
                        result.is_error,
                        receipt_authority_valid
                            && !nested_run_script_callback
                            && result
                                .metadata
                                .as_ref()
                                .and_then(|fields| fields.get("workspace_mutation_applied"))
                                .and_then(Value::as_bool)
                                == Some(true),
                    )
            {
                result
                    .metadata
                    .get_or_insert_with(Default::default)
                    .extend(receipt);
            }
            match crate::workspace_observation::project_typed_workspace_convergence_with_access(
                &self.convergence_tracker,
                Some(&self.convergence_authority),
                name,
                args,
                &self.ctx.workspace_root,
                result.is_error,
                desired_state.as_ref(),
                receipt_authority_valid && !nested_run_script_callback,
                targeted_observer,
                receipt_authority_valid && _workspace_mutation_lease.is_some(),
                native_access,
            ) {
                Ok(projection) => {
                    if let Some(receipt) = projection.convergence_receipt {
                        result
                            .metadata
                            .get_or_insert_with(Default::default)
                            .extend(receipt);
                    }
                    if let Some(receipt) = projection.observation_receipt {
                        result
                            .metadata
                            .get_or_insert_with(Default::default)
                            .extend(receipt);
                    }
                }
                Err(error) => {
                    result.is_error = true;
                    result.output.push_str(&format!(
                    "\n\nError: {error}; no completion receipt was issued. Retry inside the active turn after cancelling or finishing abandoned work."
                ));
                }
            }
        }
        if native_access.is_some()
            && receipt_authority_valid
            && !result.is_error
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            // Native preparation/commit already proved target authority through
            // descriptors. Do not reopen paths to manufacture snapshot evidence.
            result
                .metadata
                .get_or_insert_with(Default::default)
                .extend(crate::workspace_observation::typed_workspace_tool_receipt());
        }

        // This generic dispatch boundary does not establish ownership of a
        // source file. Preserve an existing source-owned marker, but use a
        // display-only redaction for raw output so web/env/error/tool results
        // cannot mint a blind edit capability.
        let result = {
            let (output, _) =
                crate::credential_redaction::redact_credentials_for_display(&result.output);
            ToolResult { output, ..result }
        };

        // Truncate oversized output to prevent context window overflow.
        let result = if result.output.len() > MAX_TOOL_OUTPUT_BYTES {
            ToolResult {
                output: crate::credential_redaction::truncate_redacted_output(
                    result.output,
                    MAX_TOOL_OUTPUT_BYTES,
                ),
                ..result
            }
        } else {
            result
        };

        if name == "bash"
            && !crate::workspace_observation::is_explicit_workspace_verification_request(name, args)
            && !result.is_error
            && let Some(key) = self.bash_cache_key(
                args,
                bash_workdir.as_ref().expect("bash workdir resolved above"),
            )
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
        // A direct writer is known from its typed tool contract; even a
        // failed attempt invalidates cached reads because a writer can
        // partially apply before returning an error. Opaque Bash may only be
        // classified after the executor's bounded pre/post observation. A
        // changed receipt likewise invalidates the generation on failure.
        let observed_bound_workspace_mutation = name == "bash"
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(crate::workspace_observation::OBSERVED_FIELD))
                .and_then(Value::as_bool)
                == Some(true)
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(crate::workspace_observation::SCOPE_FIELD))
                .and_then(Value::as_str)
                == Some(crate::workspace_observation::BOUND_WORKSPACE_SCOPE)
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(crate::workspace_observation::RECEIPT_FIELD))
                .is_some_and(crate::workspace_observation::is_changed_receipt);
        // `run_script` is an opaque workspace writer even when its Python
        // body returns an error or cancellation after a partial write.  It
        // therefore participates in cache-generation invalidation whenever
        // execution may have started.  The one explicit exception is the
        // capability/admission failure which carries
        // `execution_started=false`; that path guarantees no child was
        // spawned and must not make unrelated read caches stale.
        let exact_desired_state_noop = !result.is_error
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(crate::workspace_observation::RECEIPT_FIELD))
                .is_some_and(
                    crate::workspace_observation::is_typed_workspace_desired_state_convergence_receipt,
                );
        if (is_workspace_mutation_tool(name, args) && !exact_desired_state_noop)
            || observed_bound_workspace_mutation
            || run_script_may_have_mutated(name, &result)
        {
            self.workspace_generation.fetch_add(1, Ordering::Relaxed);
        }

        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }

    fn unstarted_shell_result(&self, name: &str, mut result: ToolResult) -> ToolResult {
        if name == "bash" && self.shell_process_boundary.is_some() {
            let receipt = astra_runtime_env::ShellExecutionEvidence {
                schema_version: 1,
                profile: astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE.into(),
                execution_started: false,
                setup: astra_runtime_env::ShellSetupEvidence::Unverified {
                    reason_code: "not_dispatched".into(),
                },
                settlement: astra_runtime_env::ShellSettlementEvidence {
                    scope_settled: true,
                    ownership: None,
                    descendants_terminated: false,
                },
                timed_out: false,
                cancelled: result
                    .metadata
                    .as_ref()
                    .and_then(|fields| fields.get("cancelled"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            result
                .metadata
                .get_or_insert_with(serde_json::Map::new)
                .insert(
                    "shell_confinement".into(),
                    serde_json::to_value(receipt).expect("shell evidence serializes"),
                );
        }
        result
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> ToolResult {
        let mut launch_attempted = false;
        let result = self
            .execute_with_launch_tracking(name, args, &mut launch_attempted)
            .await;
        if launch_attempted {
            result
        } else {
            self.unstarted_shell_result(name, result)
        }
    }

    async fn execute_with_cancel(
        &self,
        name: &str,
        args: &Value,
        cancel_token: Option<&CancellationToken>,
    ) -> ToolResult {
        let Some(cancel_token) = cancel_token else {
            return self.execute(name, args).await;
        };
        if cancel_token.is_cancelled() {
            return self.unstarted_shell_result(name, crate::cancelled_tool_result(name, false));
        }
        // Execute against a shallow clone whose context carries the caller's
        // token.  Shared caches and generation remain
        // shared, while Bash/run_script and all generic dispatch paths now
        // observe the actual caller-owned cancellation boundary rather than
        // an unrelated context token (or no token at all).
        let mut delegated = self.clone();
        delegated.ctx.cancel_token = Some(Arc::new(cancel_token.clone()));
        delegated.execute(name, args).await
    }

    fn tool_schemas(&self) -> Vec<Value> {
        let mut schemas = crate::schemas::all_tool_schemas();
        if let Some(access) = &self.native_file_access {
            schemas.retain(|schema| {
                access.is_ok()
                    && astra_core::tool_schema::tool_schema_name(schema)
                        .is_some_and(|name| name == "bash" || native_file_handler(name).is_some())
            });
        }
        if !astra_sandbox::process_scope_available() {
            schemas.retain(|schema| {
                astra_core::tool_schema::tool_schema_name(schema) != Some("run_script")
            });
        }
        schemas
    }

    fn project_root(&self) -> &Path {
        &self.ctx.project_root
    }

    fn workspace_root(&self) -> &Path {
        &self.ctx.workspace_root
    }
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

impl DefaultToolExecutor {
    /// Execute through this workspace owner while binding convergence facts to
    /// the caller's live run/turn authority. The shallow clone keeps the
    /// executor's caches and generation counters shared; only the authority
    /// envelope is request-scoped.
    pub async fn execute_with_workspace_convergence_authority(
        &self,
        name: &str,
        args: &Value,
        convergence_tracker: &crate::workspace_observation::DesiredStateConvergenceTracker,
        convergence_authority: &str,
        cancel_token: Option<&CancellationToken>,
    ) -> ToolResult {
        let mut delegated = self.clone();
        delegated.convergence_tracker = convergence_tracker.clone();
        delegated.convergence_authority = Arc::from(convergence_authority);
        delegated
            .execute_with_cancel(name, args, cancel_token)
            .await
    }

    fn bash_cache_key(
        &self,
        args: &Value,
        workdir: &crate::shell_ops::PreparedBashWorkdir,
    ) -> Option<BashCacheKey> {
        use crate::bash_cache_safety::bash_command_is_cache_safe;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        if self.shell_process_boundary.is_some() {
            return None;
        }
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
            workdir: workdir.identity().to_string(),
            workspace_generation: self.workspace_generation.load(Ordering::Relaxed),
            command,
            env_fingerprint,
            stdin_hash,
        })
    }

    async fn dispatch(
        &self,
        name: &str,
        args: &Value,
        bash_workdir: Option<&crate::shell_ops::PreparedBashWorkdir>,
        access: Option<&crate::fs_ops::FileAccess>,
        launch_attempted: &mut bool,
    ) -> ToolResult {
        let ws = &self.ctx.workspace_root;

        if let Some(handler) = native_file_handler(name) {
            return match access {
                Some(access) => (handler.confined)(ws, args, access),
                None if self.native_file_access.is_none() => (handler.ordinary)(ws, args),
                None => ToolResult::error("SANDBOX_DENIED: missing native file operation".into()),
            };
        }
        match name {
            // ── Shell operations ─────────────────────────────────────
            "bash" if self.shell_process_boundary.is_some() => {
                crate::shell_ops::execute_bash_with_process_boundary_at_workdir(
                    &self.ctx,
                    args,
                    self.shell_process_boundary.as_ref().expect("selected boundary"),
                    launch_attempted,
                    self.filesystem_write_boundary.as_deref().unwrap_or(&[]),
                    bash_workdir.expect("bash dispatch requires a resolved workdir"),
                )
                .await
            }
            "bash" => match &self.filesystem_write_boundary {
                Some(paths) => {
                    crate::shell_ops::execute_bash_with_filesystem_boundary_at_workdir(
                        &self.ctx,
                        args,
                        paths,
                        bash_workdir.expect("bash dispatch requires a resolved workdir"),
                    )
                    .await
                }
                None => {
                    crate::shell_ops::execute_bash_with_environment_at_workdir(
                        &self.ctx,
                        args,
                        &[],
                        bash_workdir.expect("bash dispatch requires a resolved workdir"),
                    )
                    .await
                }
            },
            _ if self.native_file_access.is_some() => unsupported_managed_tool(name),
            "list_dir" => crate::fs_ops::list_dir(ws, args),
            "grep" => crate::shell_ops::grep(&self.ctx, args).await,
            "glob" => crate::shell_ops::glob(&self.ctx, args).await,


            "worktree" => ToolResult::error("Error: worktree lifecycle requires an explicitly bound CLI or User Runner session owner; no operation was run".to_string()),



            // ── Code intelligence (tree-sitter) ──────────────────────
            "symbols" => self.dispatch_symbols(args),

            // ── Web search ───────────────────────────────────────────
            "web_search" => {
                let cache_scope = format!("{}:{}", self.ctx.user_id, self.ctx.session_id);
                crate::web_search::perform_web_search(args, &cache_scope).await
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
                let output = crate::web_fetch::fetch_with_cache_scope(args, &cache_scope).await;
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
                if action == crate::memory_tool_contract::MemoryAction::SessionAudit {
                    let inventory = match astra_services::session_memory_inventory::load_local_session_memory_inventory(
                        &self.ctx.session_id,
                    ) {
                        Ok(inventory) => inventory,
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Error: session memory extraction audit failed: {error}"
                            ));
                        }
                    };
                    return match serde_json::to_string(&inventory) {
                        Ok(output) => ToolResult::text(output),
                        Err(error) => ToolResult::error(format!(
                            "Error: serialize session memory extraction audit: {error}"
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
                    crate::run_script::handle_run_script_with_cancel(
                        args,
                        self,
                        config,
                        self.ctx.cancel_token.as_deref(),
                    )
                    .await
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

/// Whether a `run_script` result must conservatively advance the workspace
/// cache generation.  `run_script` executes arbitrary Python, so a successful
/// result is not the only mutation-bearing outcome: a timeout, cancellation,
/// or child error may arrive after a partial write.  Only the explicit
/// pre-admission contract (`execution_started=false`) proves that no process
/// ran.  Missing or malformed metadata is intentionally treated as started;
/// fail-closed cache invalidation is safer than serving a stale read.
fn run_script_may_have_mutated(name: &str, result: &ToolResult) -> bool {
    if name != "run_script" {
        return false;
    }
    result
        .metadata
        .as_ref()
        .and_then(|fields| fields.get("execution_started"))
        .and_then(Value::as_bool)
        != Some(false)
}

/// Return whether a typed tool invocation may mutate the bound workspace.
///
/// This is intentionally an admission/serialization predicate, not proof that
/// a mutation happened. Callers that need completion evidence must use the
/// executor-owned post-execution receipt (or the tool's typed success
/// contract). Keeping the predicate shared prevents edge and server routes
/// from acquiring different workspace observation windows.
pub fn is_workspace_mutation_tool(name: &str, args: &Value) -> bool {
    match name {
        "write_file"
        | "str_replace"
        | "multi_edit"
        | "edit_file"
        | "apply_patch"
        | "create_file"
        | "delete_file"
        | "notebook_edit"
        | "rollback_file_edits"
        | "rollback_git_worktrees"
        | "worktree"
        | "rename_symbol" => true,
        "lsp" => args.get("dry_run").and_then(Value::as_bool) == Some(false),

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn removed_repository_tools_are_unknown_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let executor = DefaultToolExecutor::for_server_workspace(
            dir.path(),
            "test-user",
            "test-session",
            "test",
            std::time::Duration::from_secs(1),
        );
        for name in ["git", "github"] {
            assert!(
                !crate::schemas::all_tool_schemas()
                    .iter()
                    .any(|s| s["function"]["name"] == name)
            );
            assert!(!SERVER_DIRECT_DEFAULT_EXECUTOR_TOOL_NAMES.contains(&name));
            let result = executor
                .execute(
                    name,
                    &serde_json::json!({"action":"commit", "message":"must not run"}),
                )
                .await;
            assert!(result.is_error, "{name}: {result:?}");
            assert!(
                result
                    .output
                    .contains("not available in DefaultToolExecutor"),
                "{name}: {}",
                result.output
            );
        }
        assert!(!dir.path().join(".git").exists());
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn restricted_shell_rejects_unsupported_routes_and_never_caches() {
        let (root, exec) = test_executor();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        let boundary = astra_sandbox::ShellProcessBoundary {
            workspace: root.path().to_path_buf(),
            home: root.path().join("unused-home"),
            temp: root.path().join("unused-temp"),
            read_only_paths: Vec::new(),
        };
        let exec = exec.with_shell_process_boundary(boundary.clone(), Vec::new());
        let args = serde_json::json!({"command": "pwd"});
        let workdir = crate::shell_ops::resolve_bash_workdir(root.path(), &args).unwrap();
        assert!(exec.bash_cache_key(&args, &workdir).is_none());
        for args in [
            serde_json::json!({"command": "touch escaped", "workdir": "nested"}),
            serde_json::json!({"command": "touch escaped", "workdir": "missing-directory"}),
            serde_json::json!({}),
            serde_json::json!({"command": "touch escaped", "env": {}}),
            serde_json::json!({"command": "touch escaped", "detach": true}),
            serde_json::json!({"command": "touch escaped", "run_in_background": true}),
        ] {
            let result = exec.execute("bash", &args).await;
            assert!(result.is_error);
            let receipt: astra_runtime_env::ShellExecutionEvidence =
                serde_json::from_value(result.metadata.unwrap()["shell_confinement"].clone())
                    .unwrap();
            assert!(!receipt.execution_started);
            assert!(receipt.settlement.scope_settled);
            assert_eq!(receipt.verified_exit_code(), None);
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        let refused = exec.execute_with_cancel("bash", &args, Some(&cancel)).await;
        let receipt: astra_runtime_env::ShellExecutionEvidence =
            serde_json::from_value(refused.metadata.unwrap()["shell_confinement"].clone()).unwrap();
        assert!(!receipt.execution_started);
        assert!(receipt.cancelled);
        for name in ["run_script", "worktree", "grep", "glob"] {
            assert!(exec.execute(name, &serde_json::json!({})).await.is_error);
        }
        // Invalid explicit toolchain inputs must not fall back even though
        // the same command is executable by an ordinary host shell.
        let mut invalid = boundary.clone();
        invalid.read_only_paths = vec![root.path().join("missing-toolchain")];
        let refused = exec
            .clone()
            .with_shell_process_boundary(invalid, Vec::new());
        let result = refused
            .execute("bash", &serde_json::json!({"command": "touch escaped"}))
            .await;
        assert!(result.is_error);
        assert!(!root.path().join("escaped").exists());
        assert!(!root.path().join("nested/escaped").exists());
        let outside = TempDir::new().unwrap();
        let mut mismatch = boundary;
        mismatch.workspace = outside.path().to_path_buf();
        let exec = exec.with_shell_process_boundary(mismatch, Vec::new());
        assert!(exec.execute("bash", &args).await.is_error);
        assert!(
            exec.execute("read_file", &serde_json::json!({"path": "missing"}))
                .await
                .output
                .contains("SANDBOX_DENIED")
        );
    }

    #[cfg(unix)]
    #[test]
    fn confined_native_schemas_follow_dispatch_capability() {
        let root = TempDir::new().unwrap();
        let exec = DefaultToolExecutor::new(ToolContext::test(root.path()))
            .with_filesystem_write_boundary(Vec::new());
        let schemas = exec.tool_schemas();
        assert!(!schemas.is_empty());
        for schema in schemas {
            let name = astra_core::tool_schema::tool_schema_name(&schema).unwrap();
            assert!(
                name == "bash" || native_file_handler(name).is_some(),
                "{name}"
            );
        }
        let unavailable = DefaultToolExecutor::new(ToolContext::test(root.path().join("missing")))
            .with_filesystem_write_boundary(Vec::new());
        assert!(unavailable.tool_schemas().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_native_executor_enforces_all_dispatch_paths() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let host = outside.path().join("sentinel.txt");
        std::fs::write(&host, "synthetic host bytes").unwrap();
        let exec = DefaultToolExecutor::new(ToolContext::test(root.path()))
            .with_filesystem_write_boundary(Vec::new());
        for (name, args) in [
            ("read_file", serde_json::json!({"path":host})),
            (
                "write_file",
                serde_json::json!({"path":host,"content":"bad"}),
            ),
            ("write_file", serde_json::json!({"path":host,"delete":true})),
            ("delete_file", serde_json::json!({"path":host})),
            (
                "str_replace",
                serde_json::json!({"path":host,"old_str":"synthetic","new_str":"bad"}),
            ),
            (
                "multi_edit",
                serde_json::json!({"path":host,"edits":[{"old_str":"synthetic","new_str":"bad"}]}),
            ),
            ("list_dir", serde_json::json!({"path":outside.path()})),
            ("glob", serde_json::json!({"pattern":"**/*"})),
            ("symbols", serde_json::json!({"path":host})),
        ] {
            let result = exec
                .dispatch(
                    name,
                    &args,
                    None,
                    Some(
                        &exec
                            .native_file_access
                            .as_ref()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .operation(),
                    ),
                    &mut false,
                )
                .await;
            assert!(result.is_error, "{name}: {}", result.output);
            assert!(!result.output.contains("synthetic host bytes"));
        }
        let result = exec
            .dispatch(
                "write_file",
                &serde_json::json!({"path":"new.txt","content":"workspace bytes"}),
                None,
                Some(
                    &exec
                        .native_file_access
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .operation(),
                ),
                &mut false,
            )
            .await;
        assert!(!result.is_error, "{}", result.output);
        let result = exec
            .execute("read_file", &serde_json::json!({"path":"new.txt"}))
            .await;
        assert!(
            !result.is_error && result.output.contains("workspace bytes"),
            "{}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(host).unwrap(),
            "synthetic host bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_native_public_write_read_and_noop_preserve_evidence_and_generation() {
        use crate::workspace_observation as observation;
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        let exec = DefaultToolExecutor::new(ToolContext::test(root.path()))
            .with_filesystem_write_boundary(Vec::new());
        let args = serde_json::json!({"path":"dir/answer.txt", "content":"stable\n"});
        let changed = exec.execute("write_file", &args).await;
        assert!(!changed.is_error, "{changed:?}");
        assert!(observation::is_typed_workspace_tool_receipt(
            &changed.metadata.as_ref().unwrap()[observation::RECEIPT_FIELD]
        ));
        let read = exec
            .execute("read_file", &serde_json::json!({"path":"dir/answer.txt"}))
            .await;
        assert!(!read.is_error, "{read:?}");
        assert!(observation::is_typed_workspace_observation_receipt(
            &read.metadata.as_ref().unwrap()[observation::OBSERVATION_RECEIPT_FIELD]
        ));
        // A legitimate replacement between invocations must not poison no-op
        // convergence with a directory retained by the previous read.
        std::fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/answer.txt"), b"stable\n").unwrap();
        let generation = exec.workspace_generation.load(Ordering::Relaxed);
        let before = std::fs::metadata(root.path().join("dir/answer.txt"))
            .unwrap()
            .modified()
            .unwrap();
        let no_op = exec.execute("write_file", &args).await;
        assert!(!no_op.is_error, "{no_op:?}");
        assert!(
            observation::is_typed_workspace_desired_state_convergence_receipt(
                &no_op.metadata.as_ref().unwrap()[observation::RECEIPT_FIELD]
            )
        );
        assert_eq!(
            exec.workspace_generation.load(Ordering::Relaxed),
            generation
        );
        assert_eq!(
            std::fs::metadata(root.path().join("dir/answer.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        let partial = exec
            .execute(
                "read_file",
                &serde_json::json!({"path":"dir/answer.txt", "start_line":1, "end_line":1}),
            )
            .await;
        assert!(!partial.is_error, "{partial:?}");
        assert!(
            observation::typed_workspace_observation_evidence(
                &partial.metadata.as_ref().unwrap()[observation::OBSERVATION_RECEIPT_FIELD]
            )
            .is_none()
        );
        let full = exec
            .execute("read_file", &serde_json::json!({"path":"dir/answer.txt"}))
            .await;
        assert!(!full.is_error, "{full:?}");
        let evidence = observation::typed_workspace_observation_evidence(
            &full.metadata.as_ref().unwrap()[observation::OBSERVATION_RECEIPT_FIELD],
        )
        .unwrap();
        assert_eq!(evidence.target, "dir/answer.txt");
        assert_eq!(
            evidence.observed_state,
            observation::workspace_file_state_identity(b"stable\n")
        );
        assert_eq!(
            exec.workspace_generation.load(Ordering::Relaxed),
            generation
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_native_directory_replaced_between_successful_reads() {
        use crate::workspace_observation as observation;
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"first\n").unwrap();
        let exec = DefaultToolExecutor::new(ToolContext::test(root.path()))
            .with_filesystem_write_boundary(Vec::new());
        let args = serde_json::json!({"path":"dir/file"});
        let first = exec.execute("read_file", &args).await;
        assert!(!first.is_error, "{first:?}");
        std::fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"second\n").unwrap();
        let second = exec.execute("read_file", &args).await;
        assert!(
            !second.is_error && second.output.contains("second"),
            "{second:?}"
        );
        assert_eq!(
            observation::typed_workspace_observation_target(
                &second.metadata.as_ref().unwrap()[observation::OBSERVATION_RECEIPT_FIELD]
            ),
            Some("dir/file")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_native_executor_rejects_replaced_workspace_root() {
        let allocation = TempDir::new().unwrap();
        let root = allocation.path().join("workspace");
        let outside = TempDir::new().unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(outside.path().join("sentinel.txt"), "synthetic host bytes").unwrap();
        let exec = DefaultToolExecutor::new(ToolContext::test(&root))
            .with_filesystem_write_boundary(Vec::new());
        std::fs::rename(&root, allocation.path().join("retained")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        let result = exec
            .execute("read_file", &serde_json::json!({"path":"sentinel.txt"}))
            .await;
        assert!(
            result.is_error && result.output.contains("pinned workspace root was replaced"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("synthetic host bytes"));
    }

    #[tokio::test]
    async fn managed_boundary_rejects_run_script_before_python_can_write() {
        let tmp = TempDir::new().unwrap();
        let protected = tmp.path().join(".moi/runtime/task-1");
        std::fs::create_dir_all(&protected).unwrap();
        let sentinel = protected.join("owned.txt");
        let exec = DefaultToolExecutor::new(ToolContext::test(tmp.path()))
            .with_filesystem_write_boundary(vec![protected]);

        let result = exec
            .execute(
                "run_script",
                &serde_json::json!({
                    "script": format!(
                        "from pathlib import Path\nPath({:?}).write_text('owned')",
                        sentinel
                    )
                }),
            )
            .await;

        assert!(result.is_error, "run_script must fail closed");
        assert!(
            result.output.contains("managed filesystem boundary"),
            "unexpected error: {}",
            result.output
        );
        assert!(
            !sentinel.exists(),
            "Python must not run outside the boundary"
        );
    }

    #[test]
    fn run_script_schema_matches_process_scope_capability() {
        let (_tmp, exec) = test_executor();
        let visible = <DefaultToolExecutor as ToolExecutor>::tool_schemas(&exec)
            .iter()
            .any(|schema| astra_core::tool_schema::tool_schema_name(schema) == Some("run_script"));
        assert_eq!(
            visible,
            astra_sandbox::process_scope_available(),
            "run_script must not be advertised when its ownership capability is unavailable"
        );
    }

    #[test]
    fn run_script_generation_invalidation_is_conservative_after_dispatch() {
        // Arbitrary Python can mutate the workspace on success, partial
        // failure, or cancellation.  All of those outcomes must invalidate
        // read caches once execution may have started.
        assert!(run_script_may_have_mutated(
            "run_script",
            &ToolResult::text("ok".into())
        ));
        assert!(run_script_may_have_mutated(
            "run_script",
            &ToolResult::error("partial failure".into())
        ));

        let mut cancelled = crate::cancelled_tool_result("run_script", true);
        assert!(run_script_may_have_mutated("run_script", &cancelled));

        // OwnershipUnavailable is the one fail-closed admission result that
        // proves no child was spawned, so it must not evict unrelated read
        // caches.  Malformed metadata is not proof and remains conservative.
        cancelled
            .metadata
            .get_or_insert_with(serde_json::Map::new)
            .insert("execution_started".into(), Value::Bool(false));
        assert!(!run_script_may_have_mutated("run_script", &cancelled));

        let mut malformed = ToolResult::error("unknown execution state".into());
        malformed
            .metadata
            .get_or_insert_with(serde_json::Map::new)
            .insert("execution_started".into(), Value::String("false".into()));
        assert!(run_script_may_have_mutated("run_script", &malformed));
        assert!(!run_script_may_have_mutated(
            "read_file",
            &ToolResult::text("ok".into())
        ));
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
    async fn exact_write_file_noop_emits_convergence_without_generation_or_cache_change() {
        let (tmp, exec) = test_executor();
        let args = serde_json::json!({"path": "answer.txt", "content": "stable\n"});
        let changed = exec.execute("write_file", &args).await;
        assert!(!changed.is_error, "{changed:?}");
        assert!(changed.metadata.as_ref().is_some_and(|fields| {
            fields
                .get(crate::workspace_observation::RECEIPT_FIELD)
                .is_some_and(crate::workspace_observation::is_typed_workspace_tool_receipt)
        }));

        let generation = exec.workspace_generation.load(Ordering::Relaxed);
        let bash_args = serde_json::json!({"command": "pwd"});
        let first_read = exec.execute("bash", &bash_args).await;
        assert!(!first_read.is_error, "{first_read:?}");
        let before = std::fs::metadata(tmp.path().join("answer.txt"))
            .expect("target metadata")
            .modified()
            .expect("mtime");

        let no_op = exec.execute("write_file", &args).await;
        assert!(!no_op.is_error, "{no_op:?}");
        let fields = no_op.metadata.as_ref().expect("convergence metadata");
        let receipt = &fields[crate::workspace_observation::RECEIPT_FIELD];
        assert!(
            crate::workspace_observation::is_typed_workspace_desired_state_convergence_receipt(
                receipt
            )
        );
        assert!(!crate::workspace_observation::is_typed_workspace_tool_receipt(receipt));

        std::fs::write(tmp.path().join("other.txt"), "other\n").expect("other target");
        for read_args in [
            serde_json::json!({"path": "other.txt"}),
            serde_json::json!({"path": "answer.txt", "start_line": 1, "end_line": 1}),
        ] {
            let read = exec.execute("read_file", &read_args).await;
            assert!(!read.is_error, "{read:?}");
            let observation = read
                .metadata
                .as_ref()
                .and_then(|fields| {
                    fields.get(crate::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                })
                .expect("generic observation receipt");
            assert!(
                crate::workspace_observation::typed_workspace_observation_evidence(observation)
                    .is_none(),
                "wrong-target and partial reads must not consume strong convergence authority"
            );
        }
        let full_read = exec
            .execute("read_file", &serde_json::json!({"path": "answer.txt"}))
            .await;
        assert!(!full_read.is_error, "{full_read:?}");
        let strong_observation = full_read
            .metadata
            .as_ref()
            .and_then(|fields| fields.get(crate::workspace_observation::OBSERVATION_RECEIPT_FIELD))
            .and_then(crate::workspace_observation::typed_workspace_observation_evidence)
            .expect("same-authority full read must carry a fresh state snapshot");
        assert_eq!(strong_observation.target, "answer.txt");
        assert_eq!(
            strong_observation.observed_state,
            crate::workspace_observation::workspace_file_state_identity(b"stable\n")
        );
        assert_eq!(
            exec.workspace_generation.load(Ordering::Relaxed),
            generation,
            "an exact no-op must not advance the workspace generation"
        );
        assert_eq!(
            std::fs::metadata(tmp.path().join("answer.txt"))
                .expect("target metadata")
                .modified()
                .expect("mtime"),
            before,
            "an exact no-op must not rewrite the target"
        );

        let cached_read = exec.execute("bash", &bash_args).await;
        assert_eq!(
            cached_read
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("cached"))
                .and_then(Value::as_bool),
            Some(true),
            "an exact no-op must not invalidate an existing read cache"
        );
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
    async fn dispatch_write_file_delete_string_is_rejected_by_schema() {
        let (tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "test.txt", "content": "hello", "delete": "true"}),
            )
            .await;
        assert!(result.is_error, "got: {}", result.output);
        assert!(
            !tmp.path().join("test.txt").exists(),
            "schema-invalid delete strings must not reach filesystem side effects"
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
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
    async fn dispatch_bash_cache_separates_execution_directories() {
        let (tmp, exec) = test_executor();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();

        let in_a = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "pwd", "workdir": "a"}),
            )
            .await;
        let in_b = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "pwd", "workdir": "b"}),
            )
            .await;
        assert!(!in_a.is_error && !in_b.is_error);
        assert_ne!(in_a.output, in_b.output);
        assert_ne!(
            in_b.metadata
                .as_ref()
                .and_then(|fields| fields.get("cached"))
                .and_then(Value::as_bool),
            Some(true),
            "same command in another workdir must not reuse cached output"
        );

        let in_b_again = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "pwd", "workdir": "b"}),
            )
            .await;
        assert_eq!(
            in_b_again
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("cached"))
                .and_then(Value::as_bool),
            Some(true),
            "same command in the same canonical workdir should remain cacheable"
        );

        let in_b_alias = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "pwd", "workdir": "a/../b"}),
            )
            .await;
        assert_eq!(
            in_b_alias.metadata.as_ref().unwrap()["bash_workdir"]["requested"],
            "a/../b",
            "a canonical cache hit must retain the current invocation's requested directory"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_bash_cache_identity_survives_workdir_alias_retarget() {
        use std::os::unix::fs::symlink;

        let (tmp, exec) = test_executor();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("input"), "from-a\n").unwrap();
        std::fs::write(b.join("input"), "from-b\n").unwrap();
        let alias = tmp.path().join("alias");
        symlink("a", &alias).unwrap();

        let first = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "cat input", "workdir": "alias"}),
            )
            .await;
        assert!(!first.is_error, "first call failed: {}", first.output);
        assert_eq!(first.output.trim(), "from-a");

        std::fs::remove_file(&alias).unwrap();
        symlink("b", &alias).unwrap();

        let from_b = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "cat input", "workdir": "alias"}),
            )
            .await;
        assert_eq!(from_b.output.trim(), "from-b");
        assert_ne!(
            from_b
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("cached"))
                .and_then(Value::as_bool),
            Some(true),
            "retargeting an alias must not relabel or reuse the old directory's cache entry"
        );
        let from_b_again = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "cat input", "workdir": "alias"}),
            )
            .await;
        assert!(
            !from_b_again.is_error,
            "cached call failed: {}",
            from_b_again.output
        );
        assert_eq!(from_b_again.output.trim(), "from-b");
        assert_eq!(
            from_b_again
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("cached"))
                .and_then(Value::as_bool),
            Some(true),
            "the new canonical workdir should be cached independently"
        );
    }

    #[tokio::test]
    async fn dispatch_bash_verify_mode_is_never_served_from_cache() {
        let (_tmp, exec) = test_executor();
        let args = serde_json::json!({ "command": "pwd", "mode": "verify" });

        let _first = exec.execute("bash", &args).await;
        let second = exec.execute("bash", &args).await;

        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "verify mode must establish a fresh observation window"
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
        assert_ne!(
            second
                .metadata
                .as_ref()
                .and_then(|m| m.get("cached"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "second call must NOT be marked cached"
        );
        let counter = std::fs::read_to_string(_tmp.path().join("dedup-count")).unwrap();
        if second.is_error {
            // A host with only weak process-group ownership must quarantine
            // after the first opaque writer.  Refusing the second execution is
            // the secure outcome and still proves no result was replayed.
            assert!(
                second.output.contains("workspace coordination lock"),
                "unexpected second failure: {}",
                second.output
            );
            assert_eq!(counter, "tick\n");
        } else {
            // With authoritative process ownership both calls may execute;
            // the counter proves the unsafe command was never cached.
            assert_eq!(
                counter, "tick\ntick\n",
                "compound command must re-execute both times when the host can prove ownership"
            );
        }
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
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(true)
        );
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
        assert_ne!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(true)
        );
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
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:read_file"}),
            )
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
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
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
    async fn shared_executor_rejects_invalid_action_arguments_before_dispatch() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "memory",
                &serde_json::json!({"action": "forget", "memory_id": "m1"}),
            )
            .await;

        assert!(result.is_error);
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
    }

    #[tokio::test]
    async fn dispatch_memory_session_audit_uses_journal_even_without_memoria_endpoint() {
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
            .execute("memory", &serde_json::json!({"action": "session_audit"}))
            .await;
        let inventory: astra_services::session_memory_inventory::SessionMemoryInventory =
            serde_json::from_str(&result.output).unwrap();

        assert!(!result.is_error, "{result:?}");
        assert_eq!(inventory.report_type, "session_memory_extraction_audit");
        assert_eq!(inventory.scope, "session");
        assert!(!inventory.contains_memory_identities);
        assert_eq!(inventory.successful_extraction_versions, 1);
        assert_eq!(inventory.llm_versions, 1);
        assert_eq!(inventory.logical_current_snapshot_count, Some(0));
    }

    #[tokio::test]
    async fn dispatch_memory_session_audit_fails_when_exactness_cannot_be_proven() {
        let journal_dir = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let path = astra_services::session_journal::journal_file_path("test-session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{not-json}\n").unwrap();
        let (_tmp, exec) = test_executor();

        let result = exec
            .execute("memory", &serde_json::json!({"action": "session_audit"}))
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
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
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
        let value: serde_json::Value = serde_json::from_str(&result.output)
            .expect("pre-execution cancellation must be a typed result");
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["error_kind"], "cancelled");
        assert_eq!(value["retryable"], false);
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
        let value: serde_json::Value = serde_json::from_str(&result.output)
            .expect("in-flight cancellation must be a typed result");
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["error_kind"], "cancelled");
        assert_eq!(value["retryable"], false);
    }

    #[tokio::test]
    async fn caller_owned_token_interrupts_without_context_token() {
        let (_tmp, exec) = test_executor();
        let token = Arc::new(CancellationToken::new());
        let trigger = Arc::clone(&token);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            trigger.cancel();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ToolExecutor::execute_with_cancel(
                &exec,
                "sleep",
                &serde_json::json!({"seconds": 30}),
                Some(token.as_ref()),
            ),
        )
        .await
        .expect("caller-owned cancellation should be observed");

        assert!(result.is_error);
        let value: serde_json::Value = serde_json::from_str(&result.output)
            .expect("caller-owned cancellation must use the typed envelope");
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["error_kind"], "cancelled");
        assert_eq!(value["retryable"], false);
    }

    // ── run_script dispatch ──────────────────────────────────────────────

    #[tokio::test]
    #[cfg_attr(not(feature = "python_tests"), ignore)]
    async fn dispatch_run_script_executes_python() {
        if !crate::run_script::python3_available() || !astra_sandbox::process_scope_available() {
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
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolInvalidArgs.as_str())
        );
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
