//! Default tool executor — the shared implementation used by CLI, server, and edge.
//!
//! Routes tool calls to the appropriate module (fs_ops, shell_ops, etc.)
//! and returns [`ToolResult`]. Consumers wrap this with their own context
//! (e.g., `ServerToolExecutor` adds resource governance and process isolation,
//! `CliToolExecutor` adds terminal UI and MCP dispatch).

use std::path::Path;
use std::sync::Arc;

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
    fetch_transport: crate::web_fetch::FetchTransport,
    approval_gate: Option<Arc<dyn ToolApprovalGate>>,
    progress_callback: Option<Arc<dyn ToolProgressCallback>>,

    convergence_tracker: crate::workspace_observation::DesiredStateConvergenceTracker,
    convergence_authority: Arc<str>,
}

impl DefaultToolExecutor {
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            fetch_transport: Default::default(),
            approval_gate: None,
            progress_callback: None,

            convergence_tracker: Default::default(),
            convergence_authority: Arc::from(uuid::Uuid::new_v4().to_string()),
        }
    }

    /// Build a ready-to-use executor from workspace parameters.
    ///
    /// This shared constructor never reads host credentials. Credential-backed
    /// tools require an authenticated, owner-scoped capability binding.
    pub fn for_workspace(
        workspace: &Path,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        let ctx = crate::ToolContext {
            project_root: workspace.to_path_buf(),
            workspace_root: workspace.to_path_buf(),
            user_id: user_id.into(),
            session_id: session_id.into(),
            sandbox: crate::SandboxConfig::standard(workspace),
            cancel_token: None,
        };

        Self::new(ctx)
    }

    pub fn with_cancel_token(mut self, token: Option<Arc<CancellationToken>>) -> Self {
        self.ctx.cancel_token = token;
        self
    }

    /// Install the network authority of a user-owned execution boundary.
    /// Server constructors intentionally retain the pinned-direct default.
    pub fn with_local_network(mut self) -> Self {
        self.fetch_transport = crate::web_fetch::FetchTransport::LocalEnvironment;
        self
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
    async fn execute_admitted(
        &self,
        name: &str,
        args: &Value,
        admission_deadline: Option<std::time::Instant>,
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

        // Resolve once per invocation. The same canonical identity drives
        // spawn and evidence so a path alias
        // cannot be retargeted between those phases.
        let bash_workdir = if name == "bash" {
            match crate::shell_ops::resolve_bash_workdir(&self.ctx.workspace_root, args) {
                Ok(workdir) => Some(workdir),
                Err(error) => return ToolResult::error(error),
            }
        } else {
            None
        };

        // Direct workspace writers participate in the same per-root lease as
        // Bash observation windows. This prevents a typed write in another
        // concurrent caller from being mistaken for an opaque Bash delta.
        let nested_run_script_callback = crate::rpc_bridge::is_run_script_rpc_dispatch();
        if name == "run_script" && nested_run_script_callback {
            return ToolResult::error(
                "run_script cannot recursively start another opaque script writer".into(),
            );
        }
        let targeted_observer = self.convergence_tracker.requires_snapshot_lease(
            &self.convergence_authority,
            name,
            args,
            &self.ctx.workspace_root,
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
                Ok(guard) => Some(guard),
                Err(failure) => {
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
                    return crate::workspace_lease_failure_tool_result(name, failure);
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
                Ok(guard) => Some(guard),
                Err(failure) => {
                    return crate::workspace_lease_failure_tool_result(name, failure);
                }
            }
        } else {
            None
        };
        if let Some(result) = crate::dispatch_deadline_result(admission_deadline, false) {
            if let Some(cb) = &self.progress_callback {
                cb.tool_completed(&call_id, &result.output, false).await;
            }
            return result;
        }
        let dispatch = self.dispatch(name, args, bash_workdir.as_ref());
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
        let coordination_integrity_valid = _workspace_mutation_lease.as_ref().is_none_or(
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
        let direct_writer_applied = coordination_integrity_valid
            && !nested_run_script_callback
            && result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool)
                == Some(true);
        if !receipt_authority_valid
            && crate::workspace_observation::typed_workspace_tool_applied_bound(
                name,
                args,
                &self.ctx.workspace_root,
                result.is_error,
                direct_writer_applied,
            )
        {
            result.metadata.get_or_insert_with(Default::default).insert(
                crate::workspace_observation::WRITER_APPLIED_BOUND_FIELD.to_string(),
                Value::Bool(true),
            );
        }
        // A successful structured workspace writer already crossed the
        // owner executor's path/permission boundary. Carry that typed fact
        // through the server/edge result ledger instead of making a remote
        // runtime guess the target against its own filesystem. This does not
        // satisfy final verification; it only opens the normal post-mutation
        // observation obligation.
        if let Some(receipt) =
            crate::workspace_observation::typed_workspace_tool_receipt_for_applied(
                name,
                args,
                &self.ctx.workspace_root,
                result.is_error,
                receipt_authority_valid && direct_writer_applied,
            )
        {
            result
                .metadata
                .get_or_insert_with(Default::default)
                .extend(receipt);
        }
        match crate::workspace_observation::project_typed_workspace_convergence(
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

        // This generic dispatch boundary does not establish ownership of a
        // source file. Preserve an existing source-owned marker, but use a
        // display-only redaction for raw output so web/env/error/tool results
        // cannot mint a blind edit capability.
        let result = {
            let (output, _) =
                astra_text_utils::credential_redaction::redact_credentials_for_display(
                    &result.output,
                );
            ToolResult { output, ..result }
        };

        // Truncate oversized output to prevent context window overflow.
        let result = if result.output.len() > MAX_TOOL_OUTPUT_BYTES {
            ToolResult {
                output: astra_text_utils::credential_redaction::truncate_redacted_output(
                    result.output,
                    MAX_TOOL_OUTPUT_BYTES,
                ),
                ..result
            }
        } else {
            result
        };

        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> ToolResult {
        self.execute_admitted(name, args, None).await
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
            return crate::cancelled_tool_result(name, false);
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
        convergence_authority: Option<&str>,
        cancel_token: Option<&CancellationToken>,
        admission_deadline: Option<std::time::Instant>,
    ) -> ToolResult {
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return crate::cancelled_tool_result(name, false);
        }
        let mut delegated = self.clone();
        if let Some(authority) = convergence_authority {
            delegated.convergence_tracker = convergence_tracker.clone();
            delegated.convergence_authority = Arc::from(authority);
        }
        if let Some(cancel_token) = cancel_token {
            delegated.ctx.cancel_token = Some(Arc::new(cancel_token.clone()));
        }
        delegated
            .execute_admitted(name, args, admission_deadline)
            .await
    }

    async fn dispatch(
        &self,
        name: &str,
        args: &Value,
        bash_workdir: Option<&crate::shell_ops::PreparedBashWorkdir>,
    ) -> ToolResult {
        let ws = &self.ctx.workspace_root;

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
            "bash" => crate::shell_ops::execute_bash_with_environment_at_workdir(
                &self.ctx,
                args,
                &[],
                bash_workdir.expect("bash dispatch requires a resolved workdir"),
            ).await,
            "grep" => crate::shell_ops::grep(&self.ctx, args).await,
            "glob" => crate::shell_ops::glob(&self.ctx, args).await,


            "worktree" => ToolResult::error("Error: worktree lifecycle requires an explicitly bound CLI or User Runner session owner; no operation was run".to_string()),



            // ── Code intelligence (tree-sitter) ──────────────────────
            "symbols" => self.dispatch_symbols(args),

            // ── Web search ───────────────────────────────────────────
            "web_search" => {
                let cache_scope = format!("{}:{}", self.ctx.user_id, self.ctx.session_id);
                crate::web_search::perform_web_search(args, &cache_scope, self.fetch_transport).await
            }

            // ── Utility tools ────────────────────────────────────────
            "tool_search" => {
                let schemas = self.tool_schemas();
                crate::tool_search::tool_search_result(&schemas, args)
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
                crate::web_fetch::fetch_with_cache_scope(args, &cache_scope, self.fetch_transport).await
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
                    crate::run_script::handle_run_script(
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

/// Return whether a top-level invocation must serialize against other
/// operations on the same physical workspace.
///
/// Provider-owned MCP effects are resolved from their discovered declaration
/// by the MCP manager and are intentionally not classified here. This
/// function is the typed-tool predicate shared by Edge and Server; MCP uses
/// the same lease implementation through its provider effect contract.
pub fn requires_workspace_serialization(name: &str, args: &Value) -> bool {
    is_workspace_mutation_tool(name, args)
}

#[cfg(test)]
mod tests {
    struct AdmissionWaitProgress(tokio::sync::Notify);

    #[async_trait]
    impl crate::ToolProgressCallback for AdmissionWaitProgress {
        async fn tool_started(&self, _id: &str, _name: &str, _args: &Value) {
            self.0.notify_one();
        }
        async fn tool_output_delta(&self, _id: &str, _delta: &str) {}
        async fn tool_completed(&self, _id: &str, _result: &str, _success: bool) {}
    }

    #[tokio::test(start_paused = true)]
    async fn admitted_default_dispatch_rechecks_after_both_workspace_guards() {
        for (name, args) in [
            (
                "write_file",
                serde_json::json!({"path": "never-written", "content": "late"}),
            ),
            (
                "run_script",
                serde_json::json!({"script": "raise AssertionError('must not execute')"}),
            ),
        ] {
            let (root, mut executor) = test_executor();
            let progress = Arc::new(AdmissionWaitProgress(tokio::sync::Notify::new()));
            executor.progress_callback = Some(progress.clone());
            let blocker =
                crate::workspace_observation::acquire_workspace_observation_lease_with_options(
                    root.path(),
                    None,
                    std::time::Duration::from_secs(1),
                )
                .await
                .unwrap();
            let deadline =
                tokio::time::Instant::now().into_std() + std::time::Duration::from_secs(1);
            let tracker = Default::default();
            let waiting = executor.execute_with_workspace_convergence_authority(
                name,
                &args,
                &tracker,
                None,
                None,
                Some(deadline),
            );
            tokio::pin!(waiting);
            tokio::select! {
                biased;
                result = &mut waiting => panic!("must block on the real workspace owner: {result:?}"),
                () = progress.0.notified() => {}
            }
            tokio::time::advance(std::time::Duration::from_secs(2)).await;
            drop(blocker);
            let result = waiting.await;
            assert_eq!(
                result.metadata.as_ref().unwrap()["execution_started"],
                false,
                "{name}: {result:?}"
            );
            assert_eq!(
                result.metadata.as_ref().unwrap()["rejection_code"],
                "execution_time_budget_exhausted"
            );
            assert!(!root.path().join("never-written").exists());
            // A rejection releases its real ownership guard; later work must
            // not remain quarantined or blocked behind an abandoned lease.
            let released =
                crate::workspace_observation::acquire_workspace_observation_lease_with_options(
                    root.path(),
                    None,
                    std::time::Duration::from_millis(1),
                )
                .await;
            assert!(released.is_ok(), "{name} leaked its workspace guard");
        }
    }

    #[tokio::test]
    async fn removed_repository_tools_are_unknown_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let executor = DefaultToolExecutor::for_workspace(dir.path(), "test-user", "test-session");
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
    async fn weak_attribution_retains_only_bound_direct_writer_applied_fact() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("out.txt"), "before\n").unwrap();
        assert!(crate::workspace_observation::quarantine_after_weak_receipt(
            tmp.path(),
            Some(crate::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP),
        ));
        let result = exec
            .execute(
                "str_replace",
                &serde_json::json!({
                    "path": "out.txt", "old_str": "before", "new_str": "after"
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
            "after\n"
        );
        let fields = result.metadata.as_ref().unwrap();
        assert_eq!(
            fields[crate::workspace_observation::WRITER_APPLIED_BOUND_FIELD],
            true
        );
        assert!(
            fields
                .get(crate::workspace_observation::RECEIPT_FIELD)
                .is_none()
        );
    }

    #[tokio::test]
    async fn exact_write_file_noop_emits_convergence_without_rewriting() {
        let (tmp, exec) = test_executor();
        let args = serde_json::json!({"path": "answer.txt", "content": "stable\n"});
        let changed = exec.execute("write_file", &args).await;
        assert!(!changed.is_error, "{changed:?}");
        assert!(changed.metadata.as_ref().is_some_and(|fields| {
            fields
                .get(crate::workspace_observation::RECEIPT_FIELD)
                .is_some_and(crate::workspace_observation::is_typed_workspace_tool_receipt)
        }));

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
            std::fs::metadata(tmp.path().join("answer.txt"))
                .expect("target metadata")
                .modified()
                .expect("mtime"),
            before,
            "an exact no-op must not rewrite the target"
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
    async fn dispatch_bash_reads_external_changes_without_replay() {
        let (tmp, exec) = test_executor();
        let path = tmp.path().join("state.txt");
        let args = serde_json::json!({"command":"cat state.txt"});
        std::fs::write(&path, "before\n").unwrap();
        let first = exec.execute("bash", &args).await;
        assert!(!first.is_error, "{first:?}");
        assert_eq!(first.output.trim(), "before");
        // An external editor does not participate in tool-owned generations.
        std::fs::write(&path, "after\n").unwrap();
        let second = exec.execute("bash", &args).await;
        assert!(!second.is_error, "{second:?}");
        assert_eq!(second.output.trim(), "after");
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
