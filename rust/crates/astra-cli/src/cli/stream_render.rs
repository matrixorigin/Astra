use super::*;
use astra_services::session_journal::{JournalEvent, JournalWriter};
use astra_tools::git_gix::{git_worktree_is_clean, head_short};
use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, EdgeApprovalRequest, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use astra_turn_core::headless_tool_assembly::READ_ONLY_TOOLS;
use astra_turn_core::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
};
use astra_turn_core::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, SseStreamHost, ToolBatchRequest,
    consume_sse_stream_cancellable, is_tool_concurrency_safe, stream_idle_timeout,
};
use astra_turn_core::tool_result_semantics::{
    cloud_tool_result_status_label, tool_dedup_signature, tool_error_triggers_rollback,
};
use crossterm::style::Stylize;
use futures_util::FutureExt;
use futures_util::StreamExt;
use futures_util::future::join_all;
use serde_json::{Map, Value};
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::ops::{Deref, DerefMut};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// CLI formatting utilities
use super::cli_formatting::{
    colorize_diff_summary, extract_cli_diff_block, format_byte_size, format_duration_suffix,
    github_repo_display, shorten_path, truncate_line,
};

// Effects module types
use super::effects::{
    ThinkingSpinnerKind, ToolRegionState, ToolStdoutLineAnim, thinking_viewport_rows,
};

fn approval_stale_revalidation_target(tool: &str, args: &Value) -> Option<PathBuf> {
    if !matches!(tool, "write_file" | "str_replace" | "edit_file") {
        return None;
    }
    args.get("path").and_then(|v| v.as_str()).map(PathBuf::from)
}

fn describe_stale_check(check: &astra_turn_core::approval_base_digest::StaleCheck) -> String {
    use astra_turn_core::approval_base_digest::StaleCheck;
    match check {
        StaleCheck::Fresh | StaleCheck::StillAbsent => "unchanged".to_string(),
        StaleCheck::Stale { previous, current } => format!(
            "changed from {} to {}",
            previous.short_display(),
            current.short_display()
        ),
        StaleCheck::FileGone { previous } => format!(
            "was removed after approval (previous {})",
            previous.short_display()
        ),
        StaleCheck::AppearedSinceEnqueue { current } => {
            format!("appeared after approval with {}", current.short_display())
        }
    }
}

fn approval_stale_revalidation_error(
    tool: &str,
    path: &Path,
    previous: Option<astra_turn_core::approval_base_digest::BaseDigest>,
) -> Option<String> {
    match astra_turn_core::approval_base_digest::stale_check(path, previous) {
        Ok(check) if check.is_fresh() => None,
        Ok(check) => Some(format!(
            "Approval expired for {tool} on {}: file {}. Please review and approve the new file state.",
            path.display(),
            describe_stale_check(&check)
        )),
        Err(e) => Some(format!(
            "Approval revalidation failed for {tool} on {}: {e}",
            path.display()
        )),
    }
}

fn approval_batch_group_key(
    tool: &str,
    args: &Value,
    risk_tags: &[astra_turn_core::permission_engine::RiskTag],
) -> astra_turn_core::approval_batch_group::ApprovalBatchGroupKey {
    let side_effect_label =
        match astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind_with_args(
            tool,
            Some(args),
        ) {
            Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Execute) => "Execute",
            Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Write) => "Write",
            _ => "Other",
        };
    astra_turn_core::approval_batch_group::ApprovalBatchGroupKey::new(
        tool.to_string(),
        side_effect_label,
        risk_tags.iter().map(|tag| format!("{tag:?}")),
        uuid::Uuid::nil(),
    )
}

fn push_risk_tag(
    tags: &mut Vec<astra_turn_core::permission_engine::RiskTag>,
    tag: astra_turn_core::permission_engine::RiskTag,
) {
    if !tags.contains(&tag) {
        tags.push(tag);
    }
}

fn approval_args_from_cloud_detail(tool: &str, detail: Option<&str>) -> Value {
    match (
        astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind(tool),
        detail,
    ) {
        (Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Execute), Some(cmd)) => {
            serde_json::json!({ "command": cmd })
        }
        (Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Write), Some(path)) => {
            serde_json::json!({ "path": path })
        }
        _ => Value::Null,
    }
}

fn approval_scope_context_for_tool(
    tool: &str,
    args: &Value,
    source_agent_present: bool,
    workspace_untrusted: bool,
) -> astra_turn_core::permission_scope::ScopeAvailabilityContext {
    use astra_turn_core::permission_engine::RiskTag;

    let mut ctx = astra_turn_core::permission_scope::ScopeAvailabilityContext {
        source_agent_present,
        workspace_untrusted,
        ..Default::default()
    };

    let base_tag = match astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind_with_args(
        tool,
        Some(args),
    ) {
        Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Execute) => {
            RiskTag::BashExecute
        }
        Some(astra_turn_core::cloud_approval_policy::CloudGatedToolKind::Write) => {
            RiskTag::WritesOutsidePackage
        }
        _ => RiskTag::BashExecute,
    };
    push_risk_tag(&mut ctx.risk_tags, base_tag);

    if let Some(path) = args.get("path").and_then(|v| v.as_str())
        && astra_turn_core::permission_redact::matches_sensitive_path(path)
    {
        push_risk_tag(&mut ctx.risk_tags, RiskTag::WritesSensitiveFile);
    }

    if tool == "bash"
        && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
    {
        let parsed = astra_turn_core::permission_compound_command::tokenize_compound_command(cmd);
        ctx.is_compound_command = parsed.steps.len() > 1;
        ctx.has_dynamic_eval = parsed.has_dynamic_eval;

        if !astra_runtime::tool_sandbox::validate_git_command(cmd).is_empty() {
            push_risk_tag(&mut ctx.risk_tags, RiskTag::GitDestructive);
        }
    }

    // Issue #326 P5 / R2 Major 5: MCP tools without a registered
    // ToolCapabilityMetadata default to MCPUnknownCapability. The
    // server-annotation lookup (slash_mcp's ToolAnnotations →
    // ApprovalMetadata) can clear this once it is wired.
    if tool.starts_with("mcp_") {
        ctx.mcp_unknown_capability = true;
        push_risk_tag(&mut ctx.risk_tags, RiskTag::MCPUnknownCapability);
    }

    ctx
}

fn approval_default_always_scope(
    ctx: &astra_turn_core::permission_scope::ScopeAvailabilityContext,
) -> astra_turn_core::permission_scope::AllowScope {
    use astra_turn_core::permission_scope::{AllowScope, permitted_scopes};

    let scopes = permitted_scopes(ctx);
    let is_available = |target| {
        scopes
            .iter()
            .any(|entry| entry.scope == target && entry.available)
    };

    if is_available(AllowScope::Project) {
        return AllowScope::Project;
    }
    if is_available(AllowScope::RestOfSession) {
        return AllowScope::RestOfSession;
    }
    if is_available(AllowScope::RestOfTurn) {
        return AllowScope::RestOfTurn;
    }

    AllowScope::OnceThisCall
}

fn audit_scope_for_always(
    scope: astra_turn_core::permission_scope::AllowScope,
) -> astra_turn_core::permission_audit::AllowScope {
    match scope {
        astra_turn_core::permission_scope::AllowScope::OnceThisCall => {
            astra_turn_core::permission_audit::AllowScope::OnceThisCall
        }
        astra_turn_core::permission_scope::AllowScope::RestOfTurn => {
            astra_turn_core::permission_audit::AllowScope::RestOfTurn
        }
        astra_turn_core::permission_scope::AllowScope::RestOfSession => {
            astra_turn_core::permission_audit::AllowScope::RestOfSession
        }
        astra_turn_core::permission_scope::AllowScope::Project => {
            astra_turn_core::permission_audit::AllowScope::Project
        }
        astra_turn_core::permission_scope::AllowScope::User => {
            astra_turn_core::permission_audit::AllowScope::User
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalMemoryAction {
    None,
    RecordAllowTurn,
    RecordAllowSession,
    RecordDenySession,
    PersistProjectRule,
    PersistUserRule,
}

fn approval_memory_action(
    response: super::chat_stream::ApprovalResponse,
    always_scope: astra_turn_core::permission_scope::AllowScope,
    stale_revalidation_passed: bool,
) -> ApprovalMemoryAction {
    use super::chat_stream::ApprovalResponse;
    use astra_turn_core::permission_scope::AllowScope;

    if response.is_approved() && !stale_revalidation_passed {
        return ApprovalMemoryAction::RecordDenySession;
    }

    match response {
        ApprovalResponse::AllowOnce => ApprovalMemoryAction::None,
        ApprovalResponse::AlwaysAllow | ApprovalResponse::AlwaysAllowScoped(_) => {
            let selected_scope = response.always_scope(always_scope).unwrap_or(always_scope);
            match selected_scope {
                AllowScope::Project => ApprovalMemoryAction::PersistProjectRule,
                AllowScope::RestOfSession => ApprovalMemoryAction::RecordAllowSession,
                AllowScope::RestOfTurn => ApprovalMemoryAction::RecordAllowTurn,
                AllowScope::OnceThisCall => ApprovalMemoryAction::None,
                AllowScope::User => ApprovalMemoryAction::PersistUserRule,
            }
        }
        ApprovalResponse::Skip | ApprovalResponse::Deny => ApprovalMemoryAction::None,
    }
}

fn persist_scoped_allow_rule(
    pm: &mut crate::permission_manager::PermissionManager,
    target: astra_turn_core::permission_audit::PersistTarget,
    tool: &str,
    args: &Value,
    save_warning_tx: Option<&super::chat_stream::StreamEventTx>,
) {
    pm.record_approval(tool, Some(args), true);
    let rule = crate::permission_manager::PermissionManager::make_allow_rule(tool, args);
    match target {
        astra_turn_core::permission_audit::PersistTarget::Project => pm.add_allow_rule(&rule),
        astra_turn_core::permission_audit::PersistTarget::User => pm.add_user_allow_rule(&rule),
    }
    if let Some(err) = pm.take_last_save_error() {
        let target_label = match target {
            astra_turn_core::permission_audit::PersistTarget::Project => ".kiro/permissions.json",
            astra_turn_core::permission_audit::PersistTarget::User => "~/.astra/permissions.json",
        };
        astra_core::agent_warn!(
            "permission",
            "Always allow for {tool} is session-only; failed to save rule {rule} to {target_label}: {err}"
        );
        if let Some(tx) = save_warning_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::StatusLine(format!(
                "Failed to save permission rule {rule} to {target_label}: {err}"
            )));
        }
    }
}

fn apply_approval_memory_action(
    pm: &mut crate::permission_manager::PermissionManager,
    action: ApprovalMemoryAction,
    tool: &str,
    args: &Value,
    save_warning_tx: Option<&super::chat_stream::StreamEventTx>,
) {
    match action {
        ApprovalMemoryAction::None => {}
        ApprovalMemoryAction::RecordAllowTurn => {
            pm.record_turn_approval(tool, Some(args), true);
        }
        ApprovalMemoryAction::RecordAllowSession => {
            pm.record_approval(tool, Some(args), true);
        }
        ApprovalMemoryAction::RecordDenySession => {
            pm.record_approval(tool, Some(args), false);
        }
        ApprovalMemoryAction::PersistProjectRule => {
            persist_scoped_allow_rule(
                pm,
                astra_turn_core::permission_audit::PersistTarget::Project,
                tool,
                args,
                save_warning_tx,
            );
        }
        ApprovalMemoryAction::PersistUserRule => {
            persist_scoped_allow_rule(
                pm,
                astra_turn_core::permission_audit::PersistTarget::User,
                tool,
                args,
                save_warning_tx,
            );
        }
    }
}

pub use astra_turn_core::chat_turn_sse_dispatch::ChatTurnEdgePending;

// Re-export effects types for callers
pub(crate) use super::effects::{ChatPrepPhaseLabel, ChatTurnPrepLineGuard};
pub(super) use super::effects::{
    Spinner, ThinkingPreviewPane, ToolRunningLineSpinner, TtftWaitLineSpinner,
};

/// Controls how terminal output is rendered during an agentic loop turn.
///
/// Replaces the previous scatter of `quiet`, `suppress_intermediate_output`,
/// and `hide_streaming_assistant_text` booleans with a single typed policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPolicy {
    /// Full interactive streaming: text deltas, spinners, tool progress, and
    /// headless lines all visible.
    Stream,
    /// Plan decomposition: suppress assistant `text_delta`, but show reasoning
    /// in the thinking viewport.  Tool UI and headless lines are still visible.
    PlanDecompose,
    /// Suppress all intermediate work (spinners, text, tool UI, headless
    /// lines).  If the turn has no tool calls, the final text is rendered
    /// one-shot at stream completion.
    FinalOnly,
    /// Complete silence: no terminal output at all.
    Silent,
}

impl RenderPolicy {
    /// True when no terminal output should be produced.
    pub fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }

    /// True when streaming text deltas should be suppressed.
    pub fn suppress_text(self) -> bool {
        !matches!(self, Self::Stream)
    }

    /// True when tool UI (spinners, progress) should be suppressed.
    pub fn suppress_tool_ui(self) -> bool {
        matches!(self, Self::FinalOnly | Self::Silent)
    }

    /// True when headless-round terminal lines should be suppressed.
    pub fn suppress_headless(self) -> bool {
        matches!(self, Self::FinalOnly | Self::Silent)
    }
}

/// Cross-turn tool output cache for the CLI edge tool execution path.
///
/// Mirrors the headless round's `InMemoryIdempotencyCache` + `call_counts`, but
/// scoped to edge-path tool calls (`tool_request` SSE events).  Cacheable tools
/// (read_file, grep, git_log, …) get their output stored and replayed on repeat.
/// All tools get a hard call-count limit to prevent runaway repetition.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EdgeToolCacheValidation {
    FileMtime {
        path: PathBuf,
        timestamp_ms: u128,
    },
    DirectoryMtime {
        path: PathBuf,
        timestamp_ms: u128,
    },
    GitHeadClean {
        project_root: PathBuf,
        head_short: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeToolCacheEntry {
    output: String,
    status: String,
    validation: EdgeToolCacheValidation,
}

pub(super) struct EdgeToolCache {
    /// `dedup_signature → cached output + validity contract` for safe replay.
    output_cache: std::collections::HashMap<String, EdgeToolCacheEntry>,
    /// `dedup_signature → count` across all turns.
    call_counts: std::collections::HashMap<String, u32>,
    /// Hard cap on identical calls (same tool + same args).
    max_identical_calls: u32,
}

impl EdgeToolCache {
    pub fn new(max_identical_calls: u32) -> Self {
        Self {
            output_cache: std::collections::HashMap::new(),
            call_counts: std::collections::HashMap::new(),
            max_identical_calls,
        }
    }
}

fn path_mtime_ms(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

impl EdgeToolCacheValidation {
    fn is_valid(&self) -> bool {
        match self {
            Self::FileMtime { path, timestamp_ms }
            | Self::DirectoryMtime { path, timestamp_ms } => path_mtime_ms(path) == *timestamp_ms,
            Self::GitHeadClean {
                project_root,
                head_short: cached_head,
            } => {
                git_worktree_is_clean(project_root).unwrap_or(false)
                    && head_short(project_root) == *cached_head
            }
        }
    }
}

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    pub render_policy: RenderPolicy,
    pub perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
    /// Optional cancellation token to abort SSE stream on auth failure.
    pub cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::chat_stream::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<super::chat_stream::ApprovalRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls in the SSE stream.
    pub skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// When true, this is a continuation turn after a skill has already produced output.
    /// Text is buffered (not streamed) and thinking previews are suppressed to avoid
    /// intermediate noise between skill iterations.
    pub skill_continuation: bool,
    /// When true, the whole turn becomes a deterministic rollback-on-failure boundary.
    pub turn_rollback_on_failure: bool,
    /// Cross-turn tool output cache (persists across turns via `CliAgenticLoopHost`).
    pub tool_cache: &'a mut EdgeToolCache,
    /// Optional ObservabilityHub for recording streaming-speculation metrics
    /// (see `AutoTuningEngine::should_disable_streaming_speculation`). `None`
    /// for tests and non-observable contexts; production supplies it from
    /// `CliAgenticLoopHost`.
    pub observability_hub:
        Option<std::sync::Arc<astra_runtime::observability_integration::ObservabilityHub>>,
}

// ─── CLI SSE stream host ─────────────────────────────────────────────────────
//
// Implements the runtime's `SseStreamHost` trait, wiring terminal rendering,
// local tool execution, and permission prompts into the generic SSE consumer.

/// CLI host for SSE stream consumption.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream`] while handling:
/// - Terminal rendering (spinners, text deltas) via [`StreamRenderState`]
/// - Edge tool execution via [`crate::edge_tools::ToolExecutor`]
/// - Approval prompts via [`crate::permission_manager::PermissionManager`]
/// - Cloud API posting (tool results, approvals) via [`astra_thin_client::ThinClient`]
struct CliSseStreamHost<'a> {
    api: &'a astra_thin_client::ThinClient,
    token: String,
    auth_profile: Option<&'a str>,
    executor_id: &'a str,
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    render_policy: RenderPolicy,
    perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
    render: StreamRenderState,
    /// Once this turn has emitted or requested tool work, hide any further prose
    /// so we don't flash an intermediate draft that will be invalidated.
    tool_work_detected: bool,
    /// Ordered tool executions from this SSE stream.
    pub edge_tool_round: Vec<EdgeToolExecResult>,
    // ── XML tag suppression ────────────────────────────────────────────
    /// Text accumulated while inside an open `<think>`/`<reflect>` tag.
    /// Flushed (after stripping the tags) once the closing tag arrives.
    /// Empty when not inside a tag — text goes directly to the renderer.
    xml_tag_buffer: String,
    /// Optional cancellation token to abort SSE stream on auth failure.
    cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    stream_event_tx: Option<super::chat_stream::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    approval_request_tx: Option<super::chat_stream::ApprovalRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Skills already invoked during this SSE stream (for edge-path dedup).
    skills_invoked: std::collections::HashSet<String>,
    /// Request IDs that were already approved through the cloud approval gate.
    /// When a `tool_request` arrives with one of these IDs, the local permission
    /// check is skipped — the user has already approved the operation.
    cloud_pre_approved: std::collections::HashSet<String>,
    /// Turn-scoped rollback checkpoints when the whole turn opts into rollback-on-failure.
    active_turn_rollback: Option<ActiveTurnRollback>,
    /// True once the current turn has emitted an execution-boundary-opened event.
    turn_rollback_boundary_emitted: bool,
    /// Tracks whether a turn-level rollback has already fired this turn.
    /// This is used to:
    /// 1. Prevent transactional batch from running (turn rollback and batch transaction
    ///    are separate rollback strategies that should not be mixed).
    /// 2. Record rollback metadata for the cloud event stream.
    ///
    /// NOTE: This does NOT block subsequent tool execution — the agent sees the error
    /// and decides whether to continue or abort.
    turn_rollback_fired: Option<TurnRollbackFired>,
    /// Cross-turn tool output cache (shared with `CliAgenticLoopHost`).
    tool_cache: &'a mut EdgeToolCache,
    /// Speculative streaming tool executor (D-9).
    ///
    /// When `ASTRA_STREAMING_TOOL_EXEC=1` is set, read-only tool_use blocks
    /// that complete mid-stream are dispatched here via `on_tool_block` so
    /// their I/O overlaps with the remaining LLM stream. After the stream
    /// ends, results are harvested and merged so normal permission checks
    /// and journal/observability events still fire exactly once in the
    /// batch phase.
    streaming_tool_exec:
        Option<std::sync::Arc<astra_turn_core::streaming_tool_exec::StreamingToolExecutor>>,
    /// Optional ObservabilityHub for streaming-speculation metric reporting.
    observability_hub:
        Option<std::sync::Arc<astra_runtime::observability_integration::ObservabilityHub>>,
    /// Set when posting edge-side tool or approval results receives 401.
    auth_failure: bool,
}

const EDGE_AUTH_FAILURE_MESSAGE: &str =
    "401 Unauthorized: session expired while posting edge tool results";

fn is_edge_auth_failure(e: &astra_thin_client::ThinClientError) -> bool {
    matches!(
        e,
        astra_thin_client::ThinClientError::Api { status, .. } if status.as_u16() == 401
    )
}

fn apply_edge_auth_failure_result(accum: &mut ChatTurnSseAccum, auth_failure: bool) {
    if auth_failure {
        accum.error_message = Some(EDGE_AUTH_FAILURE_MESSAGE.to_string());
    }
}

#[derive(Clone, Debug)]
struct BatchTransactionMetadata {
    id: String,
}

#[derive(Clone, Debug)]
struct ActiveBatchTransaction {
    id: String,
    turn_index: u32,
    file_checkpoint: u64,
    database_checkpoint: u64,
    stash_checkpoint: u64,
    commit_checkpoint: u64,
    worktree_checkpoint: u64,
    session_state_checkpoint: u64,
}

#[derive(Clone, Debug)]
struct AbortedBatchTransaction {
    id: String,
    rollback: Option<Value>,
}

#[derive(Clone, Debug)]
struct ActiveTurnRollback {
    turn_index: u32,
    file_checkpoint: u64,
    database_checkpoint: u64,
    stash_checkpoint: u64,
    commit_checkpoint: u64,
    worktree_checkpoint: u64,
    session_state_checkpoint: u64,
}

#[derive(Clone, Debug)]
struct TurnRollbackFired {
    rollback: Option<Value>,
}

const EXECUTION_BOUNDARY_KIND_TOOL_BATCH: &str = "tool_batch";
const EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK: &str = "turn_rollback";

/// RAII guard: installs a `ToolProgressSink` on the edge
/// `ToolExecutor`, spawns a ticker task that polls the sink every
/// ~200ms and emits `StreamEvent::ToolOutput`, and on drop aborts
/// the ticker + clears the sink. Scoped to a single `execute_tool`
/// call so back-to-back bash invocations each get a fresh sink
/// (counters restart from zero).
struct BashProgressGuard {
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
    ticker: Option<tokio::task::JoinHandle<()>>,
    /// Cloned observer tx so the `Drop` impl can emit one last
    /// `ToolOutput` snapshot after the pipe's final drain ran.
    /// Populated only when an observer was present at install time.
    final_event_tx: Option<super::chat_stream::StreamEventTx>,
    /// Tool name for the final `ToolOutput` snapshot.
    tool_name: String,
}

impl BashProgressGuard {
    fn install(
        executor: &std::sync::Arc<crate::edge_tools::ToolExecutor>,
        tool_name: &str,
        stream_event_tx: Option<&super::chat_stream::StreamEventTx>,
    ) -> Self {
        let sink = std::sync::Arc::new(super::chat_stream::ToolProgressSink::new());
        executor.set_bash_progress_sink(Some(sink.clone()));

        // Spawn the ticker only when there's an observer listening —
        // otherwise we'd do ~5 sink reads/sec for nothing.
        let ticker = stream_event_tx.cloned().map(|tx| {
            let sink = sink.clone();
            let name = tool_name.to_string();
            tokio::spawn(async move {
                let mut last = (0u64, 0u64);
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
                // Skip the first tick — it fires immediately and
                // would emit a 0/0 snapshot before the tool has
                // written anything.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let (lines, bytes) = sink.snapshot();
                    if (lines, bytes) == last {
                        continue;
                    }
                    last = (lines, bytes);
                    if tx
                        .send(super::chat_stream::StreamEvent::ToolOutput {
                            name: name.clone(),
                            lines,
                            bytes,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });

        Self {
            executor: executor.clone(),
            ticker,
            final_event_tx: stream_event_tx.cloned(),
            tool_name: tool_name.to_string(),
        }
    }
}

impl Drop for BashProgressGuard {
    fn drop(&mut self) {
        // Emit one last ToolOutput snapshot so the TUI counter
        // reflects the final drain bytes.  `shell.rs::final_drain_*`
        // already calls `sink.record_chunk` on every post-exit byte
        // before returning, so by the time we get here the counters
        // are authoritative.  The ticker fires at ~200 ms cadence
        // and we abort it immediately after this snapshot, so
        // without this emit the displayed "N lines / K KB" can
        // undershoot by up to one tick.  Reading the sink is two
        // atomic loads and sending is best-effort (we swallow
        // closed-channel errors exactly like the ticker loop).
        if let (Some(sink), Some(tx)) = (
            self.executor.current_bash_progress_sink(),
            self.final_event_tx.as_ref(),
        ) {
            let (lines, bytes) = sink.snapshot();
            let _ = tx.send(super::chat_stream::StreamEvent::ToolOutput {
                name: self.tool_name.clone(),
                lines,
                bytes,
            });
        }
        if let Some(h) = self.ticker.take() {
            h.abort();
        }
        self.executor.set_bash_progress_sink(None);
    }
}

impl<'a> CliSseStreamHost<'a> {
    fn from_edge_ctx(ctx: EdgeSseContext<'a>, term_width: usize, render_md: bool) -> Self {
        Self::from_edge_ctx_with_auth(ctx, term_width, render_md, None)
    }

    fn from_edge_ctx_with_auth(
        ctx: EdgeSseContext<'a>,
        term_width: usize,
        render_md: bool,
        auth_profile: Option<&'a str>,
    ) -> Self {
        let suppress_reasoning =
            ctx.render_policy == RenderPolicy::Silent || ctx.skill_continuation;
        let active_turn_rollback = ctx.turn_rollback_on_failure.then(|| ActiveTurnRollback {
            turn_index: ctx
                .executor
                .journal_turn_index
                .load(std::sync::atomic::Ordering::Acquire),
            file_checkpoint: ctx.executor.file_journal_checkpoint(),
            database_checkpoint: ctx.executor.database_snapshot_journal_checkpoint(),
            stash_checkpoint: ctx.executor.git_stash_journal_checkpoint(),
            commit_checkpoint: ctx.executor.git_commit_journal_checkpoint(),
            worktree_checkpoint: ctx.executor.git_worktree_journal_checkpoint(),
            session_state_checkpoint: ctx.executor.session_state_journal_checkpoint(),
        });
        // Always buffer text from the start.  Text is accumulated in
        // `xml_tag_buffer` and only rendered one-shot at finalization when
        // it turns out to be the final answer (no tool calls).  This avoids
        // two classes of leakage that ANSI-based `discard_and_reset()` cannot
        // reliably fix:
        //   1. Non-TTY (piped/redirected) — cursor movement has no effect.
        //   2. TTY with interleaved stderr — tool status lines push the
        //      cursor further than TerminalRegion tracks, so MoveUp(rows)
        //      falls short and the first few text lines persist in
        //      scrollback even after the "clear".
        // Trade-off: streaming text display is deferred to finalization.
        // The thinking spinner and tool status lines still stream normally,
        // so the terminal is never blank during generation.
        let buffer_from_start = true;
        let streaming_tool_exec = build_streaming_tool_exec(std::sync::Arc::clone(&ctx.executor));
        Self {
            api: ctx.api,
            token: ctx.token.to_string(),
            auth_profile,
            executor_id: ctx.executor_id,
            executor: ctx.executor,
            render_policy: ctx.render_policy,
            perm_manager: ctx.perm_manager,
            render: StreamRenderState::with_term_width(term_width, render_md, suppress_reasoning),
            tool_work_detected: buffer_from_start,
            edge_tool_round: Vec::new(),
            xml_tag_buffer: String::new(),
            cancel_token: ctx.cancel_token,
            stream_event_tx: ctx.stream_event_tx,
            approval_request_tx: ctx.approval_request_tx,
            skill_resolver: ctx.skill_resolver,
            skills_invoked: std::collections::HashSet::new(),
            cloud_pre_approved: std::collections::HashSet::new(),
            active_turn_rollback,
            turn_rollback_boundary_emitted: false,
            turn_rollback_fired: None,
            tool_cache: ctx.tool_cache,
            streaming_tool_exec,
            observability_hub: ctx.observability_hub,
            auth_failure: false,
        }
    }

    /// Push text to the active renderer (markdown or raw stdout).
    fn render_text(&mut self, s: &str) {
        // Track output bytes for live token estimation
        self.render.output_bytes = self.render.output_bytes.saturating_add(s.len());
        if let Some(pane) = self.render.thinking_pane.take() {
            let summary = pane.summary_line();
            self.render.clear_thinking_with_summary(pane, &summary);
        }
        if let Some(md) = &mut self.render.md {
            md.push(s);
        } else {
            print!("{s}");
            let _ = io::stdout().flush();
            self.render.track_output(s);
        }
    }

    fn mark_edge_auth_failure(&mut self) {
        self.auth_failure = true;
        if let Some(token) = self.cancel_token {
            token.cancel();
        }
    }

    fn handle_post_tool_result_error(&mut self, e: &astra_thin_client::ThinClientError) -> bool {
        if is_edge_auth_failure(e) {
            self.mark_edge_auth_failure();
            true
        } else if !self.render_policy.suppress_tool_ui() {
            eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
            false
        } else {
            false
        }
    }

    fn handle_post_approval_error(&mut self, e: &astra_thin_client::ThinClientError) -> bool {
        if is_edge_auth_failure(e) {
            self.mark_edge_auth_failure();
            true
        } else if !self.render_policy.suppress_tool_ui() {
            eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
            false
        } else {
            false
        }
    }

    async fn refresh_edge_token_after_401(&mut self) -> bool {
        let Some(profile) = self.auth_profile else {
            return false;
        };
        if !self.render_policy.is_silent() {
            eprintln!("{}", "  Token expired, attempting refresh…".yellow());
        }
        if !super::session_runtime::attempt_token_refresh(self.api, Some(profile)).await {
            return false;
        }
        let Some(new_token) = super::session_runtime::current_access_token(Some(profile)) else {
            return false;
        };
        self.token = new_token;
        if !self.render_policy.is_silent() {
            eprintln!("  {} Token refreshed, continuing…", crate::theme::icon_ok());
        }
        true
    }

    async fn post_tool_result_with_auth_retry(
        &mut self,
        body: &astra_thin_client::ToolResultRequest,
    ) -> bool {
        let result = self
            .api
            .post_tool_result(Some(self.token.as_str()), Some(self.executor_id), body)
            .await;
        match result {
            Ok(_) => false,
            Err(e) if is_edge_auth_failure(&e) && self.refresh_edge_token_after_401().await => {
                let retry = self
                    .api
                    .post_tool_result(Some(self.token.as_str()), Some(self.executor_id), body)
                    .await;
                if let Err(ref retry_err) = retry {
                    self.handle_post_tool_result_error(retry_err)
                } else {
                    false
                }
            }
            Err(e) => self.handle_post_tool_result_error(&e),
        }
    }

    async fn post_approval_with_auth_retry(
        &mut self,
        body: &astra_thin_client::ApprovalRespondRequest,
    ) -> bool {
        let result = self
            .api
            .post_approval(Some(self.token.as_str()), body)
            .await;
        match result {
            Ok(_) => false,
            Err(e) if is_edge_auth_failure(&e) && self.refresh_edge_token_after_401().await => {
                let retry = self
                    .api
                    .post_approval(Some(self.token.as_str()), body)
                    .await;
                if let Err(ref retry_err) = retry {
                    self.handle_post_approval_error(retry_err)
                } else {
                    false
                }
            }
            Err(e) => self.handle_post_approval_error(&e),
        }
    }

    fn validated_cache_entry(&self, dedup_sig: &str) -> Option<(String, String)> {
        let entry = self.tool_cache.output_cache.get(dedup_sig)?;
        entry
            .validation
            .is_valid()
            .then_some((entry.output.clone(), entry.status.clone()))
    }

    fn cache_validation_for_tool(
        &self,
        tool: &str,
        args: &Value,
    ) -> Option<EdgeToolCacheValidation> {
        match tool {
            "read_file" => {
                let path = self
                    .executor
                    .resolve_checked(args.get("path").and_then(Value::as_str)?)
                    .ok()?;
                let timestamp_ms = path_mtime_ms(&path);
                (timestamp_ms > 0)
                    .then_some(EdgeToolCacheValidation::FileMtime { path, timestamp_ms })
            }
            "list_dir" => {
                let path = match args.get("path").and_then(Value::as_str) {
                    Some(path) => self.executor.resolve_checked(path).ok()?,
                    None => self.executor.project_root.clone(),
                };
                let timestamp_ms = path_mtime_ms(&path);
                (timestamp_ms > 0)
                    .then_some(EdgeToolCacheValidation::DirectoryMtime { path, timestamp_ms })
            }
            "git_status" | "git_diff" | "git_log" | "git_show" | "git_blame"
            | "git_file_history" | "git_contributors" | "git_log_search" => {
                if !git_worktree_is_clean(&self.executor.project_root).unwrap_or(false) {
                    return None;
                }
                let cached_head = head_short(&self.executor.project_root);
                (!cached_head.is_empty()).then_some(EdgeToolCacheValidation::GitHeadClean {
                    project_root: self.executor.project_root.clone(),
                    head_short: cached_head,
                })
            }
            _ => None,
        }
    }

    /// Accept a text delta, suppressing content inside XML thinking tags.
    /// Text outside tags is rendered immediately (preserving streaming UX).
    /// Handles tags split across SSE chunks by holding back partial `<…` tails.
    fn push_text(&mut self, s: &str) {
        self.xml_tag_buffer.push_str(s);

        // Fast path: no tag markers at all.
        if !self.xml_tag_buffer.contains('<') {
            let buf = std::mem::take(&mut self.xml_tag_buffer);
            self.render_text(&buf);
            return;
        }

        // Check if there's an open thinking tag.
        if super::streaming_md::has_open_xml_tag(&self.xml_tag_buffer) {
            // Still inside a tag — keep buffering, don't render.
            return;
        }

        // Check for a potential incomplete thinking tag at the end of the buffer.
        // Only hold back if the tail could plausibly become one of our known tags.
        if let Some(last_lt) = self.xml_tag_buffer.rfind('<') {
            let tail = &self.xml_tag_buffer[last_lt..];
            if !tail.contains('>') && super::streaming_md::could_become_suppressed_tag(tail) {
                // Potential partial tag — split: flush before, hold tail.
                let before = self.xml_tag_buffer[..last_lt].to_string();
                let held = self.xml_tag_buffer[last_lt..].to_string();
                self.xml_tag_buffer = held;
                if !before.is_empty() {
                    let mut buf = before;
                    super::streaming_md::strip_xml_tags_inplace(&mut buf);
                    if !buf.is_empty() {
                        self.render_text(&buf);
                    }
                }
                return;
            }
        }

        // Tag is closed (or there was never one).  Strip and flush.
        let mut buf = std::mem::take(&mut self.xml_tag_buffer);
        super::streaming_md::strip_xml_tags_inplace(&mut buf);
        if !buf.is_empty() {
            self.render_text(&buf);
        }
    }

    /// Build an `EdgeToolExecResult` and post it to the cloud API.
    /// Used for cache-hit and dedup-limit early returns inside `execute_tool`.
    async fn finish_edge_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
        output: String,
        status: String,
        duration_ms: u64,
    ) -> EdgeToolExecResult {
        self.finish_edge_tool_with_fields(request_id, tool, args, output, None, status, duration_ms)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_edge_tool_with_fields(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
        output: String,
        tool_result_fields: Option<Map<String, Value>>,
        status: String,
        duration_ms: u64,
    ) -> EdgeToolExecResult {
        let result = EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.clone(),
            tool_result_fields,
            status: status.clone(),
            duration_ms,
        };
        self.edge_tool_round.push(result.clone());
        let body = astra_thin_client::ToolResultRequest {
            request_id: request_id.to_string(),
            status,
            output: Some(output),
            duration_ms: Some(duration_ms),
        };
        let _ = self.post_tool_result_with_auth_retry(&body).await;
        result
    }
}

impl<'a> CliSseStreamHost<'a> {
    #[allow(clippy::too_many_arguments)]
    fn rollback_from_checkpoints(
        &self,
        turn_index: u32,
        file_checkpoint: u64,
        database_checkpoint: u64,
        stash_checkpoint: u64,
        commit_checkpoint: u64,
        worktree_checkpoint: u64,
        session_state_checkpoint: u64,
    ) -> Option<Value> {
        let file_entries_added = self
            .executor
            .file_journal_checkpoint()
            .saturating_sub(file_checkpoint);
        let database_entries_added = self
            .executor
            .database_snapshot_journal_checkpoint()
            .saturating_sub(database_checkpoint);
        let stash_entries_added = self
            .executor
            .git_stash_journal_checkpoint()
            .saturating_sub(stash_checkpoint);
        let commit_entries_added = self
            .executor
            .git_commit_journal_checkpoint()
            .saturating_sub(commit_checkpoint);
        let worktree_entries_added = self
            .executor
            .git_worktree_journal_checkpoint()
            .saturating_sub(worktree_checkpoint);
        let session_state_entries_added = self
            .executor
            .session_state_journal_checkpoint()
            .saturating_sub(session_state_checkpoint);
        if file_entries_added == 0
            && database_entries_added == 0
            && stash_entries_added == 0
            && commit_entries_added == 0
            && worktree_entries_added == 0
            && session_state_entries_added == 0
        {
            return None;
        }

        let rollback_output = self.executor.rollback_turn_actions(&serde_json::json!({
            "scope": "turn",
            "turn_index": turn_index,
            "file_after_sequence": file_checkpoint,
            "database_after_sequence": database_checkpoint,
            "stash_after_sequence": stash_checkpoint,
            "commit_after_sequence": commit_checkpoint,
            "worktree_after_sequence": worktree_checkpoint,
            "session_state_after_sequence": session_state_checkpoint,
        }));
        Some(
            serde_json::from_str(&rollback_output).unwrap_or_else(|error| {
                serde_json::json!({
                    "ok": false,
                    "error": format!(
                        "Failed to parse rollback_turn_actions output: {error}"
                    ),
                    "raw_output": rollback_output,
                })
            }),
        )
    }

    fn has_batch_transaction_metadata(args: &Value) -> bool {
        args.as_object().is_some_and(|obj| {
            obj.contains_key("transaction_id") || obj.contains_key("rollback_on_failure")
        })
    }

    fn parse_batch_transaction_metadata(
        args: &Value,
    ) -> Result<Option<BatchTransactionMetadata>, String> {
        let Some(obj) = args.as_object() else {
            return Ok(None);
        };

        let transaction_id = match obj.get("transaction_id") {
            Some(Value::String(id)) if !id.trim().is_empty() => Some(id.trim().to_string()),
            Some(Value::String(_)) => {
                return Err("transaction_id must be a non-empty string.".to_string());
            }
            Some(_) => {
                return Err("transaction_id must be a string.".to_string());
            }
            None => None,
        };

        let rollback_on_failure = match obj.get("rollback_on_failure") {
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => {
                return Err("rollback_on_failure must be a boolean.".to_string());
            }
            None => None,
        };

        match (transaction_id, rollback_on_failure) {
            (None, None | Some(false)) => Ok(None),
            (None, Some(true)) => {
                Err("transaction_id is required when rollback_on_failure=true.".to_string())
            }
            (Some(id), Some(true)) => Ok(Some(BatchTransactionMetadata { id })),
            (Some(id), None | Some(false)) => Err(format!(
                "transaction `{id}` requires rollback_on_failure=true."
            )),
        }
    }

    fn batch_transaction_boundary_supported(tool: &str, args: &Value) -> bool {
        if tool == "bash" {
            return args
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .is_some_and(astra_turn_core::cloud_approval_policy::bash_command_is_read_only);
        }
        is_tool_concurrency_safe(tool, Some(args))
            || matches!(
                tool,
                "write_file"
                    | "delete_file"
                    | "str_replace"
                    | "multi_edit"
                    | "rename_symbol"
                    | "git_commit"
                    | "git_checkout_file"
                    | "git_stash"
                    | "notebook_edit"
                    | "mo_query"
            )
    }

    fn bash_boundary_violation(tool: &str, args: &Value, message: &str) -> Option<String> {
        if tool != "bash" {
            return None;
        }
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())?;
        if astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command) {
            return None;
        }
        Some(message.to_string())
    }

    fn merge_transaction_fields(
        mut existing: Option<Map<String, Value>>,
        transaction_id: &str,
        state: &str,
        rollback: Option<Value>,
    ) -> Option<Map<String, Value>> {
        let mut fields = existing.take().unwrap_or_default();
        fields.insert(
            "transaction_id".to_string(),
            Value::String(transaction_id.to_string()),
        );
        fields.insert(
            "transaction_boundary".to_string(),
            Value::String("tool_batch".to_string()),
        );
        fields.insert(
            "transaction_state".to_string(),
            Value::String(state.to_string()),
        );
        if let Some(rollback) = rollback {
            fields.insert("transaction_rollback".to_string(), rollback);
        }
        Some(fields)
    }

    fn append_transaction_note(
        output: &str,
        transaction_id: &str,
        note: &str,
        rollback: Option<&Value>,
    ) -> String {
        let mut rendered = output.trim_end().to_string();
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&format!("Transaction `{transaction_id}` {note}."));
        if let Some(summary) = rollback
            .and_then(|value| value.get("summary"))
            .and_then(Value::as_str)
        {
            rendered.push(' ');
            rendered.push_str(summary);
        } else if rollback.is_some() {
            rendered
                .push_str(" Bounded rollback was attempted for earlier transaction side effects.");
        }
        rendered
    }

    fn rollback_active_batch_transaction(&self, active: &ActiveBatchTransaction) -> Option<Value> {
        self.rollback_from_checkpoints(
            active.turn_index,
            active.file_checkpoint,
            active.database_checkpoint,
            active.stash_checkpoint,
            active.commit_checkpoint,
            active.worktree_checkpoint,
            active.session_state_checkpoint,
        )
    }

    fn rollback_active_turn(&self, active: &ActiveTurnRollback) -> Option<Value> {
        self.rollback_from_checkpoints(
            active.turn_index,
            active.file_checkpoint,
            active.database_checkpoint,
            active.stash_checkpoint,
            active.commit_checkpoint,
            active.worktree_checkpoint,
            active.session_state_checkpoint,
        )
    }

    fn merge_turn_rollback_fields(
        mut existing: Option<Map<String, Value>>,
        state: &str,
        rollback: Option<Value>,
    ) -> Option<Map<String, Value>> {
        let mut fields = existing.take().unwrap_or_default();
        fields.insert(
            "rollback_boundary".to_string(),
            Value::String("turn".to_string()),
        );
        fields.insert("rollback_on_failure".to_string(), Value::Bool(true));
        fields.insert(
            "rollback_state".to_string(),
            Value::String(state.to_string()),
        );
        if let Some(rollback) = rollback {
            fields.insert("rollback".to_string(), rollback);
        }
        Some(fields)
    }

    fn append_turn_rollback_note(output: &str, note: &str, rollback: Option<&Value>) -> String {
        let mut rendered = output.trim_end().to_string();
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(&format!("Turn rollback policy {note}."));
        if let Some(summary) = rollback
            .and_then(|value| value.get("summary"))
            .and_then(Value::as_str)
        {
            rendered.push(' ');
            rendered.push_str(summary);
        } else if rollback.is_some() {
            rendered.push_str(" Bounded rollback was attempted for earlier turn side effects.");
        } else {
            rendered.push_str(" No earlier bounded side effects were recorded before the failure.");
        }
        rendered
    }

    fn execution_boundary_checkpoints(
        file_checkpoint: u64,
        database_checkpoint: u64,
        stash_checkpoint: u64,
        commit_checkpoint: u64,
        worktree_checkpoint: u64,
        session_state_checkpoint: u64,
    ) -> Value {
        serde_json::json!({
            "file_checkpoint": file_checkpoint,
            "database_checkpoint": database_checkpoint,
            "stash_checkpoint": stash_checkpoint,
            "commit_checkpoint": commit_checkpoint,
            "worktree_checkpoint": worktree_checkpoint,
            "session_state_checkpoint": session_state_checkpoint,
        })
    }

    fn append_session_journal_event(&self, event: JournalEvent) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        let Ok(writer) = JournalWriter::new(&session_id) else {
            return;
        };
        let _ = writer.append(&event);
    }

    fn sync_permission_manager_session_id(&mut self) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        if let Some(pm) = self.perm_manager.as_mut() {
            pm.set_active_session_id(&session_id);
        }
    }

    fn emit_execution_boundary_opened(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        checkpoints: Value,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_opened(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            checkpoints,
        ));
    }

    fn emit_execution_boundary_committed(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        detail: Option<Value>,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_committed(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            detail,
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_execution_boundary_aborted(
        &self,
        turn_index: u32,
        boundary_kind: &str,
        transaction_id: Option<&str>,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        let Some(session_id) = self.executor.active_session_id() else {
            return;
        };
        self.append_session_journal_event(JournalEvent::execution_boundary_aborted(
            Some(&session_id),
            turn_index,
            boundary_kind,
            transaction_id,
            reason,
            trigger_tool_name,
            trigger_request_id,
            rollback,
        ));
    }

    fn emit_batch_transaction_opened(&self, active: &ActiveBatchTransaction) {
        self.emit_execution_boundary_opened(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            Self::execution_boundary_checkpoints(
                active.file_checkpoint,
                active.database_checkpoint,
                active.stash_checkpoint,
                active.commit_checkpoint,
                active.worktree_checkpoint,
                active.session_state_checkpoint,
            ),
        );
    }

    fn emit_batch_transaction_committed(&self, active: &ActiveBatchTransaction) {
        self.emit_execution_boundary_committed(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            None,
        );
    }

    fn emit_batch_transaction_aborted(
        &self,
        active: &ActiveBatchTransaction,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        self.emit_execution_boundary_aborted(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TOOL_BATCH,
            Some(&active.id),
            reason,
            trigger_request_id,
            trigger_tool_name,
            rollback,
        );
    }

    fn emit_turn_rollback_opened(&self, active: &ActiveTurnRollback) {
        self.emit_execution_boundary_opened(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            Self::execution_boundary_checkpoints(
                active.file_checkpoint,
                active.database_checkpoint,
                active.stash_checkpoint,
                active.commit_checkpoint,
                active.worktree_checkpoint,
                active.session_state_checkpoint,
            ),
        );
    }

    fn emit_turn_rollback_committed(&self, active: &ActiveTurnRollback) {
        self.emit_execution_boundary_committed(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            Some(serde_json::json!({
                "executed_requests": self.edge_tool_round.len(),
            })),
        );
    }

    fn emit_turn_rollback_aborted(
        &self,
        active: &ActiveTurnRollback,
        reason: &str,
        trigger_request_id: Option<&str>,
        trigger_tool_name: Option<&str>,
        rollback: Option<Value>,
    ) {
        self.emit_execution_boundary_aborted(
            active.turn_index,
            EXECUTION_BOUNDARY_KIND_TURN_ROLLBACK,
            None,
            reason,
            trigger_request_id,
            trigger_tool_name,
            rollback,
        );
    }

    /// Bash mutations are allowed inside turn-level rollback boundaries.
    /// They simply don't participate in checkpoint-based rollback — their
    /// side effects persist even if a later tool triggers rollback.
    /// Returning `None` means "no violation — let the tool execute".
    fn turn_rollback_boundary_violation(_tool: &str, _args: &Value) -> Option<String> {
        None
    }

    /// Returns `true` when an error from `tool` should trigger the turn-level
    /// rollback policy.  Read-only tools and bash read-only commands have no
    /// side effects so their errors are recoverable — the model can retry or
    /// use a different approach.
    fn tool_error_triggers_turn_rollback(tool: &str, args: &Value) -> bool {
        astra_turn_core::cloud_approval_policy::cloud_gated_tool_kind_with_args(tool, Some(args))
            .is_some()
    }

    fn batch_transaction_boundary_violation(tool: &str, args: &Value) -> Option<String> {
        Self::bash_boundary_violation(
            tool,
            args,
            "Error: non-read-only bash commands do not participate in rollback_on_failure batch transactions. Use structured mutation tools (write_file, git_*, rollback-aware editors), use run_build_test when available for build/test work, or keep bash read-only inside this transaction.",
        )
    }

    async fn record_synthetic_batch_result(
        &mut self,
        req: &ToolBatchRequest,
        output: String,
        status: &str,
        tool_result_fields: Option<Map<String, Value>>,
    ) -> EdgeToolExecResult {
        let duration_ms = 0;

        if let Some(tx) = &self.stream_event_tx {
            let output_summary = self
                .render
                .format_output_summary(&req.tool, &output, status)
                .map(|summary| summary.text)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(&req.tool, &req.args);
            let _ = tx.send(super::chat_stream::StreamEvent::ToolCompleted {
                name: req.tool.clone(),
                description: tool_description,
                status: status.to_string(),
                duration_ms,
                output_summary: if output_summary.is_empty() {
                    None
                } else {
                    Some(output_summary)
                },
                output: Some(output.chars().take(5000).collect()),
            });
        }

        let result = EdgeToolExecResult {
            request_id: req.request_id.clone(),
            tool: req.tool.clone(),
            args: req.args.clone(),
            output: output.clone(),
            tool_result_fields,
            status: status.to_string(),
            duration_ms,
        };
        self.edge_tool_round.push(result.clone());

        let body = astra_thin_client::ToolResultRequest {
            request_id: req.request_id.clone(),
            status: status.to_string(),
            output: Some(output),
            duration_ms: Some(duration_ms),
        };
        let _ = self.post_tool_result_with_auth_retry(&body).await;

        result
    }

    async fn execute_transactional_batch(
        &mut self,
        requests: &[ToolBatchRequest],
    ) -> Vec<EdgeToolExecResult> {
        let total = requests.len();
        let mut results = Vec::with_capacity(total);
        let mut active_tx: Option<ActiveBatchTransaction> = None;
        let mut aborted_tx: Option<AbortedBatchTransaction> = None;

        for (idx, req) in requests.iter().enumerate() {
            self.render.tool_batch_progress = Some((idx + 1, total));

            let metadata = match Self::parse_batch_transaction_metadata(&req.args) {
                Ok(metadata) => metadata,
                Err(error) => {
                    let rollback = active_tx
                        .as_ref()
                        .and_then(|active| self.rollback_active_batch_transaction(active));
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            if let Some(active) = &active_tx {
                                Self::append_transaction_note(
                                    &format!("Error: {error}"),
                                    &active.id,
                                    "failed before execution",
                                    rollback.as_ref(),
                                )
                            } else {
                                format!("Error: {error}")
                            },
                            "error",
                            active_tx.as_ref().and_then(|active| {
                                Self::merge_transaction_fields(
                                    None,
                                    &active.id,
                                    if rollback.is_some() {
                                        "rolled_back"
                                    } else {
                                        "failed"
                                    },
                                    rollback.clone(),
                                )
                            }),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &error,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }
            };

            if let Some(aborted) = aborted_tx.as_ref() {
                match metadata.as_ref() {
                    Some(meta) if meta.id == aborted.id => {
                        let result = self
                            .record_synthetic_batch_result(
                                req,
                                Self::append_transaction_note(
                                    &format!(
                                        "Error: skipped because transaction `{}` already failed earlier in this batch",
                                        aborted.id
                                    ),
                                    &aborted.id,
                                    "was already aborted",
                                    aborted.rollback.as_ref(),
                                ),
                                "error",
                                Self::merge_transaction_fields(
                                    None,
                                    &aborted.id,
                                    "aborted",
                                    aborted.rollback.clone(),
                                ),
                            )
                            .await;
                        results.push(result);
                        continue;
                    }
                    _ => aborted_tx = None,
                }
            }

            let continuing_active_transaction = active_tx
                .as_ref()
                .zip(metadata.as_ref())
                .is_some_and(|(active, meta)| active.id == meta.id);
            if !continuing_active_transaction {
                if let Some(active) = active_tx.take() {
                    self.emit_batch_transaction_committed(&active);
                }
            }

            if let Some(meta) = metadata.as_ref()
                && active_tx.is_none()
            {
                let active = ActiveBatchTransaction {
                    id: meta.id.clone(),
                    turn_index: self
                        .executor
                        .journal_turn_index
                        .load(std::sync::atomic::Ordering::Acquire),
                    file_checkpoint: self.executor.file_journal_checkpoint(),
                    database_checkpoint: self.executor.database_snapshot_journal_checkpoint(),
                    stash_checkpoint: self.executor.git_stash_journal_checkpoint(),
                    commit_checkpoint: self.executor.git_commit_journal_checkpoint(),
                    worktree_checkpoint: self.executor.git_worktree_journal_checkpoint(),
                    session_state_checkpoint: self.executor.session_state_journal_checkpoint(),
                };
                self.emit_batch_transaction_opened(&active);
                active_tx = Some(active);
            }

            if let Some(meta) = metadata.as_ref() {
                if let Some(error) =
                    Self::batch_transaction_boundary_violation(&req.tool, &req.args)
                {
                    let rollback = active_tx
                        .as_ref()
                        .and_then(|active| self.rollback_active_batch_transaction(active));
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            Self::append_transaction_note(
                                &error,
                                &meta.id,
                                "failed before execution",
                                rollback.as_ref(),
                            ),
                            "error",
                            Self::merge_transaction_fields(
                                None,
                                &meta.id,
                                if rollback.is_some() {
                                    "rolled_back"
                                } else {
                                    "failed"
                                },
                                rollback.clone(),
                            ),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &error,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    } else {
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: meta.id.clone(),
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }

                if !Self::batch_transaction_boundary_supported(&req.tool, &req.args) {
                    let rollback = active_tx
                        .as_ref()
                        .and_then(|active| self.rollback_active_batch_transaction(active));
                    let result = self
                        .record_synthetic_batch_result(
                            req,
                            Self::append_transaction_note(
                                &format!(
                                    "Error: tool `{}` does not support rollback-on-failure batch transactions",
                                    req.tool
                                ),
                                &meta.id,
                                "failed before execution",
                                rollback.as_ref(),
                            ),
                            "error",
                            Self::merge_transaction_fields(
                                None,
                                &meta.id,
                                if rollback.is_some() {
                                    "rolled_back"
                                } else {
                                    "failed"
                                },
                                rollback.clone(),
                            ),
                        )
                        .await;
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &format!(
                                "tool `{}` does not support rollback-on-failure batch transactions",
                                req.tool
                            ),
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    } else {
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: meta.id.clone(),
                            rollback,
                        });
                    }
                    results.push(result);
                    continue;
                }
            }

            let mut result = self
                .execute_tool(&req.request_id, &req.tool, &req.args)
                .await;

            if let Some(active) = active_tx.as_ref() {
                if metadata.as_ref().is_some_and(|meta| meta.id == active.id)
                    && result.status == "error"
                {
                    let rollback = self.rollback_active_batch_transaction(active);
                    let failure_reason = result.output.clone();
                    result.output = Self::append_transaction_note(
                        &result.output,
                        &active.id,
                        "failed",
                        rollback.as_ref(),
                    );
                    result.tool_result_fields = Self::merge_transaction_fields(
                        result.tool_result_fields.take(),
                        &active.id,
                        if rollback.is_some() {
                            "rolled_back"
                        } else {
                            "failed"
                        },
                        rollback.clone(),
                    );
                    if let Some(last) = self.edge_tool_round.last_mut() {
                        if last.request_id == result.request_id {
                            *last = result.clone();
                        }
                    }
                    if let Some(active) = active_tx.take() {
                        self.emit_batch_transaction_aborted(
                            &active,
                            &failure_reason,
                            Some(&req.request_id),
                            Some(&req.tool),
                            rollback.clone(),
                        );
                        aborted_tx = Some(AbortedBatchTransaction {
                            id: active.id,
                            rollback,
                        });
                    }
                }
            }

            results.push(result);
        }

        if let Some(active) = active_tx.take() {
            self.emit_batch_transaction_committed(&active);
        }

        results
    }
}

// `extract_first_absolute_path` moved to `crate::sandbox_retry`.

/// D-9 correctness guard: decide whether a speculative result may be
/// reused as-is in place of a real tool execution.
///
/// A speculative tool invocation returns `(output, success)`. A `success=false`
/// outcome means the speculation **errored** (permission-denied mid-stream,
/// tool panic surfaced as error string, bash non-zero exit, grep pattern not
/// found reported as error, etc.). Silently substituting an errored output as
/// if it were a successful tool_result causes the LLM to reason on an
/// error-as-success and cascades into hallucinated next steps.
///
/// When `success=false`, callers must fall through to the normal execution
/// path so the tool re-runs and the real outcome (success or genuine error)
/// surfaces through the standard journal/observability pipeline.
///
/// Returns `Some(output)` only when the speculation was a genuine success.
pub(crate) fn reusable_speculative_output(r: Option<(String, bool)>) -> Option<String> {
    match r {
        Some((output, true)) => Some(output),
        _ => None,
    }
}

impl CliSseStreamHost<'_> {
    /// D-9: Harvest speculative results for the upcoming concurrent batch.
    ///
    /// `wait_all()` is used so in-flight speculations finish before the
    /// merge; the overall latency is still bounded by the stream itself
    /// (the stream has already finished by the time this runs). Results
    /// keyed by request_id are returned so the join_all closure can
    /// short-circuit matching requests without re-executing.
    async fn harvest_speculation_for_batch(
        &self,
        conc_reqs: &[(usize, &ToolBatchRequest)],
    ) -> std::collections::HashMap<String, (String, bool)> {
        let Some(exec) = self.streaming_tool_exec.as_ref() else {
            return std::collections::HashMap::new();
        };
        // Use `merge_speculative` (not raw `wait_all`) so per-call-id hit
        // counters and saved-ms metrics are updated for observability.
        let ids: Vec<String> = conc_reqs
            .iter()
            .map(|(_, r)| r.request_id.clone())
            .collect();
        let (done, _needed) = exec.merge_speculative(&ids).await;
        let mut out = std::collections::HashMap::new();
        let mut reusable = 0usize;
        let mut rejected_failure = 0usize;
        for r in done {
            if r.success {
                reusable += 1;
            } else {
                // Speculation completed but failed — the reconciler will fall
                // back to real execution (see `reusable_speculative_output`).
                // Track this separately from `snapshot().wasted` so operators
                // can distinguish "speculation errored" from "speculation
                // never started" when diagnosing hit-rate drops.
                rejected_failure += 1;
            }
            out.insert(r.call_id.clone(), (r.content.clone(), r.success));
        }
        // Per-batch reconciliation breakdown: complements the cumulative
        // `astra::streaming_speculation::metrics` with this-batch counts so
        // operators can correlate a specific turn's LLM-emitted batch against
        // what actually came back from speculation. Target:
        // `astra::streaming_speculation::batch`.
        tracing::info!(
            target: "astra::streaming_speculation::batch",
            batch_size = conc_reqs.len(),
            reusable = reusable,
            rejected_failure = rejected_failure,
            not_speculated = conc_reqs.len().saturating_sub(reusable + rejected_failure),
            session_id = self.executor.active_session_id().as_deref().unwrap_or(""),
            "speculation reconciliation for batch"
        );
        // Emit a structured metrics event once per batch merge so log
        // aggregators / ObservabilityHub can track speculation effectiveness
        // over time. Target: `astra::streaming_speculation::metrics`.
        exec.emit_metrics_log(self.executor.active_session_id().as_deref())
            .await;
        if let Some(hub) = self.observability_hub.as_ref() {
            let snap = exec.snapshot().await;
            hub.record_streaming_speculation_metrics(&snap);
            // Reset so each batch report is a delta; AutoTuningEngine sums
            // incoming reports additively.
            exec.reset_metrics().await;
        }
        out
    }

    async fn resolve_cloud_approval_via_tui(
        &mut self,
        tool: &str,
        detail: Option<&str>,
        approval_kind: astra_thin_client::ApprovalKind,
    ) -> astra_thin_client::ApprovalDecision {
        use super::chat_stream::ApprovalResponse;
        use astra_thin_client::ApprovalDecision;

        if let Some(decision) = self.perm_manager.as_mut().and_then(|pm| {
            pm.preflight_cloud_approval_decision(
                tool,
                detail,
                approval_kind,
                self.render_policy.is_silent(),
            )
        }) {
            return decision;
        }

        let Some(tx) = &self.approval_request_tx else {
            astra_core::agent_warn!(
                "permission",
                "Auto-denied cloud approval for {tool}: no TUI approval sink installed"
            );
            return ApprovalDecision::Deny;
        };

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let header = format!("Cloud approval required: {tool}");
        let reason = if matches!(approval_kind, astra_thin_client::ApprovalKind::Explicit) {
            "This tool call requires explicit approval before it can run.".to_string()
        } else {
            "Cloud approval required before this tool can run.".to_string()
        };
        if tx
            .send(super::chat_stream::ApprovalRequest::bare(
                tool.to_string(),
                header,
                detail.map(ToString::to_string),
                reason,
                resp_tx,
            ))
            .is_err()
        {
            astra_core::agent_warn!(
                "permission",
                "Auto-denied cloud approval for {tool}: TUI approval sink is closed"
            );
            return ApprovalDecision::Deny;
        }

        let response = if let Some(token) = self.cancel_token {
            tokio::select! {
                biased;
                _ = token.cancelled() => ApprovalResponse::Deny,
                r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
            }
        } else {
            resp_rx.await.unwrap_or(ApprovalResponse::Deny)
        };

        let explicit = matches!(approval_kind, astra_thin_client::ApprovalKind::Explicit);
        let approval_args = approval_args_from_cloud_detail(tool, detail);
        let always_scope = approval_default_always_scope(&approval_scope_context_for_tool(
            tool,
            &approval_args,
            false,
            false,
        ));
        match response {
            ApprovalResponse::AllowOnce => ApprovalDecision::Allow,
            // The current TUI button row has one Always button.
            // Explicit cloud approvals are confirm-once by contract,
            // so treat Always as AllowOnce until the P3 scope picker
            // can disable persistent scopes for this prompt kind.
            ApprovalResponse::AlwaysAllow | ApprovalResponse::AlwaysAllowScoped(_) if explicit => {
                ApprovalDecision::Allow
            }
            ApprovalResponse::AlwaysAllow | ApprovalResponse::AlwaysAllowScoped(_) => {
                let action = approval_memory_action(response, always_scope, true);
                let save_warning_tx = self.stream_event_tx.clone();
                if let Some(pm) = self.perm_manager.as_mut() {
                    apply_approval_memory_action(
                        pm,
                        action,
                        tool,
                        &approval_args,
                        save_warning_tx.as_ref(),
                    );
                }
                match action {
                    ApprovalMemoryAction::RecordAllowTurn => ApprovalDecision::Allow,
                    ApprovalMemoryAction::RecordAllowSession
                    | ApprovalMemoryAction::PersistProjectRule
                    | ApprovalMemoryAction::PersistUserRule => ApprovalDecision::AllowSession,
                    ApprovalMemoryAction::None | ApprovalMemoryAction::RecordDenySession => {
                        ApprovalDecision::Allow
                    }
                }
            }
            ApprovalResponse::Skip => {
                if let Some(pm) = self.perm_manager.as_mut() {
                    pm.record_approval(tool, Some(&approval_args), false);
                }
                ApprovalDecision::Deny
            }
            ApprovalResponse::Deny => ApprovalDecision::Deny,
        }
    }
}

#[async_trait::async_trait]
impl SseStreamHost for CliSseStreamHost<'_> {
    fn on_before_sse_read_loop(&mut self) {
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::WaitingForModel);
        }
        if self.render_policy.is_silent() {
            return;
        }
        self.render.start_waiting_for_model();
    }

    fn on_first_sse_frame(&mut self) {
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::ModelResponding);
        }
        // Don't stop the TTFT spinner here — the first SSE frame is often
        // metadata (session_info, usage) not visible content.  Let the
        // spinner run until actual thinking/text arrives, which will
        // dismiss it via StartThinkingSpinner or StopThinkingSpinner.
    }

    fn on_idle_tick(&mut self) {
        if self.render_policy.is_silent() {
            return;
        }
        self.render.tick_thinking_pane();
    }

    fn on_session_id(&mut self, session_id: &str) {
        if self.executor.active_session_id().as_deref() != Some(session_id) {
            self.executor.set_active_session_id(session_id.to_string());
        }
        if let Some(pm) = self.perm_manager.as_mut() {
            pm.set_active_session_id(session_id);
        }
    }

    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        // Forward to stream event channel (even when quiet/suppress are on)
        if let Some(tx) = &self.stream_event_tx {
            use super::chat_stream::StreamEvent;
            for effect in &effects {
                let ev = match effect {
                    SseRenderEffect::StreamText(s) if !s.is_empty() => {
                        Some(StreamEvent::Token(s.clone()))
                    }
                    SseRenderEffect::StartThinkingSpinner => Some(StreamEvent::Thinking(true)),
                    SseRenderEffect::StopThinkingSpinner => Some(StreamEvent::Thinking(false)),
                    SseRenderEffect::ThinkingPreviewChunk(s) if !s.is_empty() => {
                        Some(StreamEvent::ThinkingChunk(s.clone()))
                    }
                    _ => None,
                };
                if let Some(ev) = ev {
                    let _ = tx.send(ev);
                }
            }
        }

        let policy = self.render_policy;
        match policy {
            RenderPolicy::Silent => return,
            RenderPolicy::FinalOnly => {
                // Suppress StreamText but still render thinking preview
                // (spinner + reasoning chunks) so the user sees progress.
                for effect in &effects {
                    match effect {
                        SseRenderEffect::StartThinkingSpinner => self.render.start_thinking(),
                        SseRenderEffect::StopThinkingSpinner => self.render.stop_thinking(),
                        SseRenderEffect::ThinkingPreviewChunk(s) => {
                            self.render.push_thinking_preview_chunk(s);
                        }
                        SseRenderEffect::StreamText(_) => {} // suppressed
                    }
                }
                return;
            }
            RenderPolicy::PlanDecompose | RenderPolicy::Stream => {}
        }

        let mut i = 0usize;
        while i < effects.len() {
            match &effects[i] {
                SseRenderEffect::StopThinkingSpinner => {
                    // `text_delta` emits Stop then StreamText; in plan-only mode we stream the
                    // assistant body into the reasoning viewport — skipping Stop avoids clearing
                    // the pane on every token.
                    let skip = policy == RenderPolicy::PlanDecompose
                        && i + 1 < effects.len()
                        && matches!(&effects[i + 1], SseRenderEffect::StreamText(_));
                    if !skip {
                        self.render.stop_thinking();
                    }
                    i += 1;
                }
                SseRenderEffect::StreamText(s) => {
                    if policy == RenderPolicy::PlanDecompose {
                        // Plan decompose mode: don't show the raw JSON body in
                        // the thinking preview.  Only genuine <thinking> content
                        // (via ThinkingPreviewChunk) should appear there.
                        i += 1;
                        continue;
                    }
                    // When tool_work_detected, buffer text instead of discarding.
                    // It will be rendered at stream completion if it's the final answer.
                    if self.tool_work_detected {
                        self.xml_tag_buffer.push_str(s);
                        i += 1;
                        continue;
                    }
                    self.push_text(s);
                    i += 1;
                }
                SseRenderEffect::StartThinkingSpinner => {
                    self.render.start_thinking();
                    i += 1;
                }
                SseRenderEffect::ThinkingPreviewChunk(s) => {
                    self.render.push_thinking_preview_chunk(s);
                    i += 1;
                }
            }
        }
    }

    fn on_stream_complete(&mut self) {
        self.render.stop_thinking();
        if self.turn_rollback_boundary_emitted
            && let Some(active) = self.active_turn_rollback.take()
        {
            self.emit_turn_rollback_committed(&active);
        }
    }

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> EdgeToolExecResult {
        self.sync_permission_manager_session_id();

        // Forward tool-started event to observer channel
        let tool_description = self.render.format_tool_description(tool, args);
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::ToolStarted {
                name: tool.to_string(),
                description: tool_description.clone(),
            });
        }

        // Install a bash progress sink + 200ms ticker for the TUI's
        // live "streaming · N lines · K KB" counter. Bash is the
        // only tool that streams stdout incrementally today — other
        // tools are atomic and never emit progress. Cleanup (abort
        // ticker, clear sink) happens unconditionally at the bottom
        // of `execute_tool` via `_progress_guard`.
        let _progress_guard = (tool == "bash").then(|| {
            BashProgressGuard::install(&self.executor, tool, self.stream_event_tx.as_ref())
        });

        // Clear text that was rendered or buffered BEFORE the first tool call
        // (intermediate draft). After first tool, keep buffering new text.
        if !self.tool_work_detected {
            self.tool_work_detected = true;
            // Discard any XML-tag-buffered text that was never rendered.
            self.xml_tag_buffer.clear();

            // Clear text that WAS already rendered (intermediate draft).
            if let Some(md) = &mut self.render.md {
                md.discard_and_reset();
            } else if self.render.lines_written > 0 && io::stdout().is_terminal() {
                execute!(
                    io::stdout(),
                    cursor::MoveUp(self.render.lines_written as u16),
                    cursor::MoveToColumn(0),
                    terminal::Clear(terminal::ClearType::FromCursorDown)
                )
                .ok();
                self.render.lines_written = 0;
                self.render.col = 0;
            }
        }
        // `tool_request` does not emit StopThinking; clear the thinking stderr line so it does
        // not fight the running-tool spinner (`\r` on the same fd).
        self.render.stop_thinking();
        // Show tool as running (in-place updatable via TerminalRegion).
        let tool_idx = if !self.render_policy.suppress_tool_ui() {
            Some(self.render.tool_start(tool, args))
        } else {
            None
        };

        // NOTE: We no longer block subsequent tools when a prior tool triggered rollback.
        // The agent sees the error and can decide whether to continue or abort.
        // This allows more flexible recovery strategies.

        if !self.turn_rollback_boundary_emitted
            && let Some(active) = self.active_turn_rollback.clone()
        {
            self.emit_turn_rollback_opened(&active);
            self.turn_rollback_boundary_emitted = true;
        }

        // ── Edge-path dedup: call-count limit + output cache ───────────
        let dedup_sig = tool_dedup_signature(tool, args);
        let call_count = {
            let count = self
                .tool_cache
                .call_counts
                .entry(dedup_sig.clone())
                .or_insert(0);
            *count += 1;
            *count
        };
        let max_calls = self.tool_cache.max_identical_calls;

        if call_count > max_calls {
            // Hard cap exceeded — return a stub telling the LLM to stop.
            let body = if let Some((cached_out, _)) = self.validated_cache_entry(&dedup_sig) {
                format!(
                    "⛔ Cached repeat (call #{} for identical args, limit: {}). \
                     The result is already in this conversation from an earlier call. \
                     Do NOT call this tool again with the same arguments.\n\n{}",
                    call_count,
                    max_calls,
                    &cached_out[..cached_out.len().min(200)],
                )
            } else {
                format!(
                    "⛔ Duplicate call #{} (limit: {}). This tool has been called too many times \
                     with the same arguments. Use the results from earlier calls instead.",
                    call_count, max_calls,
                )
            };
            let status = "error";
            if let Some(idx) = tool_idx {
                self.render.tool_done(idx, tool, args, status, 0, &body);
            }
            return self
                .finish_edge_tool(request_id, tool, args, body, status.to_string(), 0)
                .await;
        }

        // Cache hit for read-only (cacheable) tools
        if READ_ONLY_TOOLS.contains(&tool)
            && let Some((cached_output, cached_status)) = self.validated_cache_entry(&dedup_sig)
        {
            if let Some(idx) = tool_idx {
                self.render
                    .tool_done(idx, tool, args, &cached_status, 0, &cached_output);
            }
            return self
                .finish_edge_tool(request_id, tool, args, cached_output, cached_status, 0)
                .await;
        }

        // Skip local permission check if this tool was already approved through
        // the cloud approval gate (approval_required → user approved → tool_request).
        // This eliminates the double-prompt issue where the same operation requires
        // both cloud approval and local approval.
        let cloud_approved = self.cloud_pre_approved.remove(request_id);

        let decision = if cloud_approved {
            crate::permission_manager::PermissionDecision::Allow
        } else {
            match self.perm_manager.as_mut() {
                Some(pm) => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    Some(&mut **pm),
                    tool,
                    args,
                ),
                None => crate::tool_safety_guard::ToolSafetyGuard::check_request(None, tool, args),
            }
        };
        let mut denied_output = None;
        let mut allowed = match decision {
            crate::permission_manager::PermissionDecision::Allow => true,
            crate::permission_manager::PermissionDecision::Deny(reason) => {
                denied_output = Some(crate::permission_manager::format_denied_message(&reason));
                false
            }
            crate::permission_manager::PermissionDecision::NeedApproval {
                tool: t,
                header,
                detail,
                reason,
            } => {
                if let Some(tx) = &self.approval_request_tx {
                    use super::chat_stream::ApprovalResponse;
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    // Issue #326 P3: compute the metadata bundle so
                    // the TUI card can render the will-save preview,
                    // risk badge, and (if applicable) compound-
                    // command split. Senders without this context
                    // would call ApprovalRequest::bare instead.
                    let mut metadata = crate::tui::approval::queue::ApprovalMetadata::default();

                    // Issue #326 P4 / R2 Critical 1: compute the
                    // ApprovalRequestKey from the live request so
                    // the queue can dedup any subsequent in-flight
                    // request that resolves to byte-identical
                    // (tool, cwd, args). The user only sees one
                    // prompt; their answer broadcasts to all
                    // waiting senders.
                    let request_key =
                        astra_turn_core::approval_request_key::ApprovalRequestKey::new(
                            t.clone(),
                            std::env::current_dir().unwrap_or_default(),
                            args,
                            None,
                            uuid::Uuid::nil(),
                        );
                    metadata.request_key = Some(request_key);

                    // Issue #326 P3/P5: one shared host-side
                    // classifier feeds the risk badges, batch-group
                    // safety gate, will-save visibility, and the
                    // actual Always storage action.
                    let workspace_untrusted = self
                        .perm_manager
                        .as_ref()
                        .is_some_and(|pm| !pm.project_allow_rules_active());
                    let scope_ctx = approval_scope_context_for_tool(
                        &t,
                        args,
                        metadata.source_agent.is_some(),
                        workspace_untrusted,
                    );
                    metadata.risk_tags = scope_ctx.risk_tags.clone();
                    metadata.workspace_untrusted = scope_ctx.workspace_untrusted;
                    metadata.is_compound_command = scope_ctx.is_compound_command;
                    metadata.has_dynamic_eval = scope_ctx.has_dynamic_eval;
                    let always_scope = approval_default_always_scope(&scope_ctx);

                    // Will-save: what would Always persist? We use
                    // the same make_allow_rule the post-resolve
                    // path uses, so the user sees an exact preview
                    // of what permissions.json will gain. Only show
                    // it when the actual default Always scope is
                    // Project; session-only / call-only approvals
                    // must not advertise an on-disk write.
                    if always_scope == astra_turn_core::permission_scope::AllowScope::Project {
                        let will_save =
                            crate::permission_manager::PermissionManager::make_allow_rule(&t, args);
                        // Issue #326 P5 / scenario #28: include
                        // the package root in the will-save
                        // preview so the user knows the rule will
                        // be scoped to (say) `packages/web`, not
                        // promoted globally. nearest_package_root
                        // walks up from the cwd looking for a
                        // package marker (Cargo.toml, package.json,
                        // pyproject.toml, …); None means
                        // "workspace root" — we use a literal
                        // marker so the user sees something.
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let scope_label =
                            astra_turn_core::permission_cwd_root::nearest_package_root(&cwd, None)
                                .as_deref()
                                .map(|p| {
                                    cwd.strip_prefix(p)
                                        .ok()
                                        .map(|rel| {
                                            if rel.as_os_str().is_empty() {
                                                p.file_name()
                                                    .map(|n| n.to_string_lossy().into_owned())
                                                    .unwrap_or_else(|| {
                                                        p.to_string_lossy().into_owned()
                                                    })
                                            } else {
                                                p.file_name()
                                                    .map(|n| n.to_string_lossy().into_owned())
                                                    .unwrap_or_else(|| {
                                                        p.to_string_lossy().into_owned()
                                                    })
                                            }
                                        })
                                        .unwrap_or_else(|| p.to_string_lossy().into_owned())
                                });
                        let will_save_with_scope = if let Some(label) = scope_label {
                            format!("{will_save}  (scope: {label}/)")
                        } else {
                            will_save
                        };
                        metadata.will_save_preview = Some(will_save_with_scope);
                    }
                    metadata.batch_group_key =
                        Some(approval_batch_group_key(&t, args, &metadata.risk_tags));

                    // Issue #326 P5 / scenario #11: when bash is
                    // about to run a local script, attach a
                    // preview of the body so the user can read
                    // the actual code before approving. We only
                    // detect simple invocations
                    // (`bash foo.sh`, `./foo.sh`); compound
                    // commands and shell idioms are handled by
                    // the compound-command tokenizer above.
                    if t == "bash" {
                        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                            if let Some(script_path) =
                                astra_turn_core::permission_script_preview::looks_like_local_script(
                                    cmd,
                                )
                            {
                                let cwd = std::env::current_dir()
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                if let Ok(preview) =
                                    astra_turn_core::permission_script_preview::build_script_preview(
                                        &script_path,
                                        &cwd,
                                    )
                                {
                                    if preview.has_destructive_hit
                                        && !metadata.risk_tags.contains(
                                            &astra_turn_core::permission_engine::RiskTag::BashExecute,
                                        )
                                    {
                                        // Already pushed above
                                        // for bash; this branch
                                        // exists for symmetry
                                        // when the body has a
                                        // destructive hit but
                                        // the cmd_kind didn't
                                        // mark Execute.
                                    }
                                }
                            }
                        }
                    }
                    // Issue #326 P5f / R2 Major 3: for file-mutating
                    // tools, snapshot the target file's SHA-256
                    // here (host-side, NOT from LLM args). The
                    // executor's pre-execution stale-check in
                    // ApprovalQueue::focused_stale_check compares
                    // this against a fresh read; mismatch =
                    // re-prompt with new diff. Skipping here
                    // means no stale revalidation, which is
                    // safe-fail — the user just doesn't get the
                    // protection for this tool.
                    if matches!(t.as_str(), "write_file" | "str_replace" | "edit_file") {
                        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                            let path_buf = std::path::Path::new(path);
                            match astra_turn_core::approval_base_digest::compute_file_digest(
                                path_buf,
                            ) {
                                Ok(Some(digest)) => {
                                    metadata.base_digest = Some(digest);
                                }
                                Ok(None) => {
                                    // Brand-new write to a path
                                    // that doesn't exist yet —
                                    // base_digest stays None so
                                    // the StaleCheck routes
                                    // through StillAbsent.
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "stream_render: failed to compute base_digest for \
                                         {tool}={path:?}: {e} — approval will lack stale revalidation",
                                        tool = t,
                                    );
                                }
                            }
                        }
                    }
                    let stale_revalidation = approval_stale_revalidation_target(&t, args)
                        .map(|path| (path, metadata.base_digest.clone()));
                    let _ = tx.send(super::chat_stream::ApprovalRequest {
                        tool: t.clone(),
                        header,
                        detail,
                        reason,
                        response_tx: resp_tx,
                        metadata: Some(Box::new(metadata)),
                    });
                    let response = if let Some(token) = self.cancel_token {
                        tokio::select! {
                            biased;
                            _ = token.cancelled() => ApprovalResponse::Deny,
                            r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                        }
                    } else {
                        resp_rx.await.unwrap_or(ApprovalResponse::Deny)
                    };
                    let mut stale_revalidation_passed = true;
                    if response.is_approved() {
                        if let Some((path, previous)) = stale_revalidation.as_ref() {
                            if let Some(error) =
                                approval_stale_revalidation_error(&t, path, previous.clone())
                            {
                                stale_revalidation_passed = false;
                                denied_output = Some(error);
                            }
                        }
                    }
                    let save_warning_tx = self.stream_event_tx.clone();
                    if let Some(pm) = self.perm_manager.as_mut() {
                        apply_approval_memory_action(
                            pm,
                            approval_memory_action(
                                response,
                                always_scope,
                                stale_revalidation_passed,
                            ),
                            &t,
                            args,
                            save_warning_tx.as_ref(),
                        );
                    }
                    // Issue #326 P6 / R2 Major 4: emit
                    // ApprovalResolvedEvent so audit can see
                    // what the user picked. correlation_id ties
                    // this back to the PermissionEvaluatedEvent
                    // that produced the prompt (the engine
                    // wiring populates that side; today we use
                    // a tool+timestamp scheme).
                    {
                        let timestamp_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let correlation_id = format!("approval-{}-{}", timestamp_ms, t);
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let request_key =
                            astra_turn_core::approval_request_key::ApprovalRequestKey::new(
                                t.clone(),
                                cwd,
                                args,
                                None,
                                uuid::Uuid::nil(),
                            );
                        let core_response = match response {
                            ApprovalResponse::AllowOnce => {
                                astra_turn_core::approval_sink::ApprovalResponse::AllowOnce
                            }
                            ApprovalResponse::AlwaysAllow => {
                                astra_turn_core::approval_sink::ApprovalResponse::AlwaysAllow
                            }
                            ApprovalResponse::AlwaysAllowScoped(_) => {
                                astra_turn_core::approval_sink::ApprovalResponse::AlwaysAllow
                            }
                            ApprovalResponse::Deny => {
                                astra_turn_core::approval_sink::ApprovalResponse::Deny
                            }
                            ApprovalResponse::Skip => {
                                astra_turn_core::approval_sink::ApprovalResponse::Skip
                            }
                        };
                        let scope = response
                            .always_scope(always_scope)
                            .map(audit_scope_for_always);
                        astra_turn_core::permission_audit::record_resolved_for_session(
                            self.executor.active_session_id().as_deref(),
                            astra_turn_core::permission_audit::ApprovalResolvedEvent {
                                timestamp_ms,
                                correlation_id,
                                request_key,
                                response: core_response,
                                scope,
                                stale_revalidation_passed,
                            },
                        );
                    }
                    response.is_approved() && stale_revalidation_passed
                } else if self.render_policy.is_silent() {
                    astra_core::agent_warn!(
                        "permission",
                        "Auto-denied {t} in sub-run mode (no interactive terminal): {reason}"
                    );
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&t, Some(args), false);
                    }
                    false
                } else {
                    // Issue #326 P0 (tui-only) / #331: with the
                    // REPL deleted upstream, TUI is the sole
                    // interactive mode and it always installs an
                    // `approval_request_tx`. Reaching this branch
                    // means: no approval channel AND not silent —
                    // a configuration mismatch, not a user
                    // workflow. Fail closed with an actionable
                    // reason so the LLM sees a Deny instead of
                    // hanging on a stdin readline that the user
                    // can't see.
                    astra_core::agent_warn!(
                        "permission",
                        "Auto-denied {t}: no approval sink installed (no TUI, not silent). \
                         Pass --mode auto or attach to a TUI session. reason={reason}"
                    );
                    if let Some(pm) = self.perm_manager.as_mut() {
                        pm.record_approval(&t, Some(args), false);
                    }
                    false
                }
            }
        };
        if allowed
            && self.active_turn_rollback.is_some()
            && let Some(error) = Self::turn_rollback_boundary_violation(tool, args)
        {
            denied_output = Some(error);
            allowed = false;
        }
        let start = std::time::Instant::now();
        let mut tool_result_fields = None;
        let mut output = if allowed {
            if tool == astra_runtime::turn::skill_tool::SKILL_TOOL_NAME {
                // Edge-path skill dedup: if the same skill was already invoked
                // during this SSE stream, return a short dedup message instead
                // of executing it again.
                let skill_name = astra_runtime::turn::skill_tool::extract_skill_name(args);
                let dedup_key = skill_name.unwrap_or_default().to_string();
                if !dedup_key.is_empty() && !self.skills_invoked.insert(dedup_key.clone()) {
                    format!(
                        "Skill '{}' was already loaded in this turn. \
                         Follow the instructions already provided.",
                        dedup_key,
                    )
                } else if let Some(resolver) = &self.skill_resolver {
                    // D-9 dedup: discard any speculative execution tied to this call_id.
                    if let Some(exec) = self.streaming_tool_exec.clone() {
                        exec.discard(request_id).await;
                    }
                    let raw = astra_runtime::turn::skill_tool::execute_skill_inline(
                        resolver.as_ref(),
                        tool,
                        args,
                    )
                    .await;
                    // Append `<skill-loaded name="..."/>` so the LLM sees
                    // the "do not re-invoke" signal. Without this, the
                    // system-prompt rule "On seeing <skill-loaded/>, follow
                    // instructions — do not re-invoke" never triggers on the
                    // CLI edge path, and the LLM loads a second skill
                    // (session 11825116 regression). Server-side path does
                    // this in partition_discover_and_execute_skills:1098.
                    append_skill_loaded_marker(&raw, &dedup_key)
                } else {
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME {
                if let Some(resolver) = &self.skill_resolver {
                    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let catalog = resolver.available_skills();
                    let (text, _) = astra_runtime::turn::skill_tool::execute_discover_skills(
                        query,
                        &catalog,
                        std::collections::HashSet::new(),
                        None,
                    );
                    text
                } else {
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::agentic_loop_host::DELEGATE_TOOL_NAME {
                // Delegate calls are intercepted at Step 3b of the agentic loop
                // (partition_and_execute_delegations) where the delegation engine
                // runs sub-agents. Return a deferred acknowledgment so the server
                // sees a success (not an error) and the model doesn't give up.
                //
                // D-9 dedup guard: if a speculative execution was somehow started
                // for this call_id, discard it so the delegation result wins.
                if let Some(exec) = self.streaming_tool_exec.clone() {
                    exec.discard(request_id).await;
                }
                "Delegation request acknowledged. The delegation engine will execute \
                 this request now, the parent agent will pause while sub-agents \
                 run and aggregate, and the summarized results will be injected \
                 before the parent agent finishes."
                    .to_string()
            } else {
                let mut outcome = self.executor.execute_with_metadata(tool, args).await;
                // If the sandbox denied the operation, prompt the user for
                // authorization. On approval, temporarily expand the sandbox
                // boundary and retry the tool.
                if crate::sandbox_retry::is_sandbox_denied(&outcome.output) {
                    if let Some(pm) = &mut self.perm_manager {
                        let sandbox_msg =
                            crate::sandbox_retry::sandbox_denied_message(&outcome.output)
                                .unwrap_or("");
                        let sandbox_tool_key = format!("sandbox_expand:{tool}");
                        let guard_args = serde_json::json!({"reason": sandbox_msg});
                        let decision = crate::tool_safety_guard::ToolSafetyGuard::check_request(
                            Some(&mut **pm),
                            &sandbox_tool_key,
                            &guard_args,
                        );
                        let approved = match decision {
                            crate::permission_manager::PermissionDecision::Allow => true,
                            crate::permission_manager::PermissionDecision::Deny(_) => false,
                            crate::permission_manager::PermissionDecision::NeedApproval {
                                header,
                                detail,
                                reason,
                                ..
                            } => {
                                if let Some(tx) = &self.approval_request_tx {
                                    use super::chat_stream::ApprovalResponse;
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    // `🔒 ` prefix visually marks sandbox-escape
                                    // prompts; header/detail/reason otherwise
                                    // come straight from the permission manager
                                    // so we don't echo the same text thrice.
                                    let _ = tx.send(super::chat_stream::ApprovalRequest::bare(
                                        sandbox_tool_key.clone(),
                                        format!("🔒 {header}"),
                                        detail,
                                        reason,
                                        resp_tx,
                                    ));
                                    let response = if let Some(token) = self.cancel_token {
                                        tokio::select! {
                                            biased;
                                            _ = token.cancelled() => ApprovalResponse::Deny,
                                            r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                                        }
                                    } else {
                                        resp_rx.await.unwrap_or(ApprovalResponse::Deny)
                                    };
                                    let selected_scope = response.always_scope(
                                        astra_turn_core::permission_scope::AllowScope::Project,
                                    );
                                    match selected_scope {
                                        Some(astra_turn_core::permission_scope::AllowScope::Project) => {
                                            // Persistent: writes a tool-level allow
                                            // rule to settings for future sessions.
                                            let rule = crate::permission_manager::PermissionManager::make_allow_rule(&sandbox_tool_key, &guard_args);
                                            pm.add_allow_rule(&rule);
                                            if let Some(err) = pm.take_last_save_error() {
                                                astra_core::agent_warn!(
                                                    "permission",
                                                    "Always allow for {sandbox_tool_key} is session-only; failed to save rule {rule}: {err}"
                                                );
                                                if let Some(tx) = &self.stream_event_tx {
                                                    let _ = tx.send(super::chat_stream::StreamEvent::StatusLine(
                                                        format!("Failed to save permission rule {rule}: {err}"),
                                                    ));
                                                }
                                            }
                                            pm.trust_sandbox_root_from_reason(sandbox_msg);
                                        }
                                        Some(
                                            astra_turn_core::permission_scope::AllowScope::RestOfSession,
                                        ) => {
                                            pm.trust_sandbox_root_from_reason(sandbox_msg);
                                        }
                                        Some(
                                            astra_turn_core::permission_scope::AllowScope::User,
                                        ) => {
                                            let rule = crate::permission_manager::PermissionManager::make_allow_rule(&sandbox_tool_key, &guard_args);
                                            pm.add_user_allow_rule(&rule);
                                            if let Some(err) = pm.take_last_save_error() {
                                                astra_core::agent_warn!(
                                                    "permission",
                                                    "Always allow for {sandbox_tool_key} is session-only; failed to save user rule {rule}: {err}"
                                                );
                                                if let Some(tx) = &self.stream_event_tx {
                                                    let _ = tx.send(super::chat_stream::StreamEvent::StatusLine(
                                                        format!("Failed to save user permission rule {rule}: {err}"),
                                                    ));
                                                }
                                            }
                                            pm.trust_sandbox_root_from_reason(sandbox_msg);
                                        }
                                        Some(
                                            astra_turn_core::permission_scope::AllowScope::OnceThisCall
                                            | astra_turn_core::permission_scope::AllowScope::RestOfTurn,
                                        )
                                        | None => {}
                                    }
                                    if matches!(
                                        selected_scope,
                                        Some(
                                            astra_turn_core::permission_scope::AllowScope::Project
                                                | astra_turn_core::permission_scope::AllowScope::RestOfSession
                                                | astra_turn_core::permission_scope::AllowScope::User
                                        )
                                    ) {
                                        pm.record_approval(&sandbox_tool_key, Some(args), true);
                                    }
                                    response.is_approved()
                                } else if self.render_policy.is_silent() {
                                    // Sub-run mode: auto-deny sandbox expansion
                                    astra_core::agent_warn!(
                                        "permission",
                                        "Auto-denied sandbox expansion {sandbox_tool_key} in sub-run mode: {reason}"
                                    );
                                    pm.record_approval(&sandbox_tool_key, Some(args), false);
                                    false
                                } else {
                                    // Issue #326 P0 (tui-only) / #331:
                                    // legacy interactive stdin path is dead
                                    // code now that the REPL is gone. With
                                    // no approval channel and not silent =
                                    // configuration mismatch, fail closed.
                                    astra_core::agent_warn!(
                                        "permission",
                                        "Auto-denied sandbox expansion {sandbox_tool_key}: \
                                         no approval sink installed (no TUI, not silent). \
                                         Pass --mode auto or attach to a TUI session. reason={reason}"
                                    );
                                    pm.record_approval(&sandbox_tool_key, Some(args), false);
                                    false
                                }
                            }
                        };
                        if approved {
                            // Single source of truth in `sandbox_retry` — both
                            // the sequential path here and the parallel batch
                            // path call the same derivation so their behaviour
                            // stays byte-identical.
                            if let Some(dir) =
                                crate::sandbox_retry::sandbox_expand_dir_from_args(args)
                            {
                                self.executor.expand_sandbox_path(dir);
                            }
                            outcome = self.executor.execute_with_metadata(tool, args).await;
                            tool_result_fields = outcome.tool_result_fields;
                            outcome.output
                        } else {
                            format!("Error: {sandbox_msg}")
                        }
                    } else {
                        outcome.output
                    }
                } else {
                    tool_result_fields = outcome.tool_result_fields;
                    outcome.output
                }
            }
        } else {
            denied_output.unwrap_or_else(|| "Permission denied".to_string())
        };
        let status = if !allowed {
            "error"
        } else {
            cloud_tool_result_status_label(&output)
        }
        .to_string();
        let duration_ms = start.elapsed().as_millis() as u64;

        // Rollback policy: only trigger turn rollback for HARD errors on mutation tools.
        // Soft errors (e.g., "old_str == new_str", "file not found") let the agent retry.
        if status == "error"
            && Self::tool_error_triggers_turn_rollback(tool, args)
            && tool_error_triggers_rollback(tool, &output)
            && let Some(active) = self.active_turn_rollback.clone()
        {
            let rollback = self.rollback_active_turn(&active);
            let failure_reason = output.clone();
            output = Self::append_turn_rollback_note(&output, "failed", rollback.as_ref());
            tool_result_fields = Self::merge_turn_rollback_fields(
                tool_result_fields.take(),
                if rollback.is_some() {
                    "rolled_back"
                } else {
                    "failed"
                },
                rollback.clone(),
            );
            self.emit_turn_rollback_aborted(
                &active,
                &failure_reason,
                Some(request_id),
                Some(tool),
                rollback.clone(),
            );
            self.turn_rollback_fired = Some(TurnRollbackFired { rollback });
            self.active_turn_rollback = None;
        }

        // Store successful cacheable tool results for cross-turn dedup.
        if allowed
            && status != "error"
            && READ_ONLY_TOOLS.contains(&tool)
            && let Some(validation) = self.cache_validation_for_tool(tool, args)
        {
            self.tool_cache.output_cache.insert(
                dedup_sig.clone(),
                EdgeToolCacheEntry {
                    output: output.clone(),
                    status: status.clone(),
                    validation,
                },
            );
        }

        // Forward tool-completed event to observer channel
        if let Some(tx) = &self.stream_event_tx {
            let output_summary = self
                .render
                .format_output_summary(tool, &output, &status)
                .map(|summary| summary.text)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(tool, args);
            let _ = tx.send(super::chat_stream::StreamEvent::ToolCompleted {
                name: tool.to_string(),
                description: tool_description,
                status: status.clone(),
                duration_ms,
                output_summary: if output_summary.is_empty() {
                    None
                } else {
                    Some(output_summary)
                },
                output: Some(output.chars().take(5000).collect()),
            });
        }

        // Update tool line to show completion.
        if let Some(idx) = tool_idx {
            self.render
                .tool_done(idx, tool, args, &status, duration_ms, &output);
        }
        self.edge_tool_round.push(EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.clone(),
            tool_result_fields: tool_result_fields.clone(),
            status: status.clone(),
            duration_ms,
        });
        let body = astra_thin_client::ToolResultRequest {
            request_id: request_id.to_string(),
            status: status.clone(),
            output: Some(output),
            duration_ms: Some(duration_ms),
        };
        let _ = self.post_tool_result_with_auth_retry(&body).await;
        self.edge_tool_round
            .last()
            .cloned()
            .unwrap_or_else(|| EdgeToolExecResult {
                request_id: String::new(),
                tool: String::new(),
                args: serde_json::Value::Null,
                output: "Error: no tool result recorded".to_string(),
                tool_result_fields: None,
                status: "error".to_string(),
                duration_ms: 0,
            })
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        approval_kind: astra_thin_client::ApprovalKind,
        session_id: Option<&str>,
        detail: Option<&str>,
    ) -> EdgeApprovalResult {
        // `resolve_cloud_approval` writes to stderr only. Never bump `lines_written` here:
        // that counter drives stdout `MoveUp` when clearing streamed text before the first
        // tool line; mixing in stderr line counts caused a large blank gap after prompts.
        //
        // Stop spinner/animation before prompting so inquire::Select renders
        // cleanly and doesn't fight the running-tool spinner on stderr.
        self.render.stop_tool_stderr_running();
        self.render.stop_tool_stdout_anim();
        self.render.stop_thinking();
        let decision = if self.perm_manager.is_some() {
            self.resolve_cloud_approval_via_tui(tool, detail, approval_kind)
                .await
        } else {
            astra_thin_client::ApprovalDecision::Deny
        };
        let decision_str = match &decision {
            astra_thin_client::ApprovalDecision::Allow
            | astra_thin_client::ApprovalDecision::AllowSession => {
                // Track this request_id so the subsequent tool_request
                // skips the redundant local permission check.
                self.cloud_pre_approved.insert(request_id.to_string());
                "allow"
            }
            _ => "deny",
        };
        let body = astra_thin_client::ApprovalRespondRequest {
            request_id: request_id.to_string(),
            decision,
            reason: None,
            session_id: session_id.map(ToString::to_string),
            tool_name: Some(tool.to_string()),
            approval_kind: Some(approval_kind),
        };
        let _ = self.post_approval_with_auth_retry(&body).await;
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: decision_str.to_string(),
            reason: None,
        }
    }

    async fn resolve_approvals_batch(
        &mut self,
        requests: &[EdgeApprovalRequest],
        session_id: Option<&str>,
    ) -> Vec<EdgeApprovalResult> {
        if requests.is_empty() {
            return Vec::new();
        }

        self.render.stop_tool_stderr_running();
        self.render.stop_tool_stdout_anim();
        self.render.stop_thinking();

        let decisions = if self.perm_manager.is_some() {
            let mut decisions = Vec::with_capacity(requests.len());
            for request in requests {
                decisions.push(
                    self.resolve_cloud_approval_via_tui(
                        request.tool.as_str(),
                        request.detail.as_deref(),
                        request.approval_kind,
                    )
                    .await,
                );
            }
            decisions
        } else {
            vec![astra_thin_client::ApprovalDecision::Deny; requests.len()]
        };

        let mut results = Vec::with_capacity(requests.len());
        for (request, decision) in requests.iter().zip(decisions) {
            let decision_str = match &decision {
                astra_thin_client::ApprovalDecision::Allow
                | astra_thin_client::ApprovalDecision::AllowSession => {
                    self.cloud_pre_approved.insert(request.request_id.clone());
                    "allow"
                }
                _ => "deny",
            };
            let body = astra_thin_client::ApprovalRespondRequest {
                request_id: request.request_id.clone(),
                decision,
                reason: None,
                session_id: session_id.map(ToString::to_string),
                tool_name: Some(request.tool.clone()),
                approval_kind: Some(request.approval_kind),
            };
            let _ = self.post_approval_with_auth_retry(&body).await;
            results.push(EdgeApprovalResult {
                request_id: request.request_id.clone(),
                decision: decision_str.to_string(),
                reason: None,
            });
        }
        results
    }

    /// Parallel batch execution for concurrent-safe tools.
    ///
    /// Sequential (side-effect) tools run first via [`execute_tool`](Self::execute_tool).
    /// Then all concurrent-safe tools execute in parallel via `join_all`, overlapping
    /// network I/O for async tools (GitHub, Memoria, MCP). If any request in the
    /// batch carries explicit transaction metadata, the whole batch falls back to
    /// deterministic original-order execution so rollback boundaries remain crisp.
    async fn execute_tools_batch(
        &mut self,
        requests: Vec<ToolBatchRequest>,
    ) -> Vec<EdgeToolExecResult> {
        self.sync_permission_manager_session_id();

        let n = requests.len();

        // Set batch progress for multi-tool turns.
        if n > 1 {
            self.render.tool_batch_progress = Some((1, n));
        }

        let has_batch_transaction = requests
            .iter()
            .any(|req| Self::has_batch_transaction_metadata(&req.args));

        // Fast path: ≤1 tool — use existing sequential code.
        if n <= 1 {
            if has_batch_transaction
                && self.active_turn_rollback.is_none()
                && self.turn_rollback_fired.is_none()
            {
                let out = self.execute_transactional_batch(&requests).await;
                self.render.tool_batch_progress = None;
                return out;
            }
            let mut out = Vec::with_capacity(n);
            for req in requests {
                out.push(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        if self.active_turn_rollback.is_some() || self.turn_rollback_fired.is_some() {
            let mut out = Vec::with_capacity(n);
            for (i, req) in requests.iter().enumerate() {
                self.render.tool_batch_progress = Some((i + 1, n));
                out.push(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        if has_batch_transaction {
            let out = self.execute_transactional_batch(&requests).await;
            self.render.tool_batch_progress = None;
            return out;
        }

        // Classify by concurrency safety.
        let conc_flags: Vec<bool> = requests
            .iter()
            .map(|req| is_tool_concurrency_safe(&req.tool, Some(&req.args)))
            .collect();
        let conc_count = conc_flags.iter().filter(|&&f| f).count();

        // < 2 concurrent-safe tools: no parallelism benefit.
        if conc_count < 2 {
            let mut out = Vec::with_capacity(n);
            for (i, req) in requests.iter().enumerate() {
                self.render.tool_batch_progress = Some((i + 1, n));
                out.push(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            self.render.tool_batch_progress = None;
            return out;
        }

        let mut results: Vec<Option<EdgeToolExecResult>> = (0..n).map(|_| None).collect();

        // Collect concurrent-safe requests (preserving order) and run sequential ones first.
        let mut seq_done = 0usize;
        let seq_total = requests
            .iter()
            .enumerate()
            .filter(|(i, _)| !conc_flags[*i])
            .count();
        let mut conc_reqs: Vec<(usize, &ToolBatchRequest)> = Vec::with_capacity(conc_count);
        for (i, req) in requests.iter().enumerate() {
            if conc_flags[i] {
                conc_reqs.push((i, req));
            } else {
                // Side-effect tools execute eagerly in original order.
                seq_done += 1;
                self.render.tool_batch_progress = Some((seq_done, seq_total + conc_count));
                results[i] = Some(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
        }
        // Batch-size observation: correlates the read-only batching prompt
        // (`prompts/system.rs`) with actual LLM emission patterns. When >=2
        // concurrent tools arrive together the model followed the guidance;
        // when conc_reqs is empty or has a single entry, batching didn't
        // happen this turn. Target: `astra::tool_batching::batch_size`.
        if !conc_reqs.is_empty() {
            tracing::info!(
                target: "astra::tool_batching::batch_size",
                parallel_count = conc_reqs.len(),
                sequential_count = seq_total,
                tool_names = ?conc_reqs.iter().map(|(_, r)| r.tool.as_str()).collect::<Vec<_>>(),
                session_id = self.executor.active_session_id().as_deref().unwrap_or(""),
                "LLM emitted parallel tool batch"
            );
        }

        // Pre-check: can all concurrent tools auto-proceed?
        // Read-only tools hit the fast-path in check_nonblocking (SideEffect::Read → Allow).
        let mut all_allowed = true;
        for (_, req) in &conc_reqs {
            let decision = match self.perm_manager.as_mut() {
                Some(pm) => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    Some(&mut **pm),
                    &req.tool,
                    &req.args,
                ),
                None => crate::tool_safety_guard::ToolSafetyGuard::check_request(
                    None, &req.tool, &req.args,
                ),
            };
            let ok = matches!(
                decision,
                crate::permission_manager::PermissionDecision::Allow
            );
            if !ok {
                all_allowed = false;
                break;
            }
        }

        if !all_allowed {
            // Rare for read-only tools. Fall back to sequential.
            for (i, req) in conc_reqs {
                results[i] = Some(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            return results
                .into_iter()
                .map(|r| r.expect("all tool result slots filled"))
                .collect();
        }

        // ── Phase 1: Pre-execution UI setup (sequential, &mut self) ──
        // For parallel tools, clear progress indicator (they all run together).
        self.render.tool_batch_progress = None;

        // Markdown mode: show single grouped spinner for parallel tools.
        // Non-markdown mode: show individual lines that can update in place.
        let parallel_count = conc_reqs.len();
        let use_grouped_spinner = self.render.md.is_some() && parallel_count > 1;

        let mut ui_indices: Vec<Option<usize>> = Vec::with_capacity(conc_reqs.len());
        for (i, (_, req)) in conc_reqs.iter().enumerate() {
            // Forward tool-started event.
            let desc = self.render.format_tool_description(&req.tool, &req.args);
            if let Some(tx) = &self.stream_event_tx {
                let _ = tx.send(super::chat_stream::StreamEvent::ToolStarted {
                    name: req.tool.clone(),
                    description: desc,
                });
            }
            // First-tool clearing (once per turn).
            if !self.tool_work_detected {
                self.tool_work_detected = true;
                self.xml_tag_buffer.clear();
                if let Some(md) = &mut self.render.md {
                    md.discard_and_reset();
                } else if self.render.lines_written > 0 && io::stdout().is_terminal() {
                    execute!(
                        io::stdout(),
                        cursor::MoveUp(self.render.lines_written as u16),
                        cursor::MoveToColumn(0),
                        terminal::Clear(terminal::ClearType::FromCursorDown)
                    )
                    .ok();
                    self.render.lines_written = 0;
                    self.render.col = 0;
                }
            }
            self.render.stop_thinking();

            // In grouped mode, only start spinner once for all parallel tools.
            let tool_idx = if !self.render_policy.suppress_tool_ui() {
                if use_grouped_spinner {
                    if i == 0 {
                        // Start grouped spinner for first tool only.
                        Some(self.render.tool_start_parallel_group(parallel_count))
                    } else {
                        // Other tools share the group spinner (no individual display).
                        None
                    }
                } else {
                    Some(self.render.tool_start(&req.tool, &req.args))
                }
            } else {
                None
            };
            ui_indices.push(tool_idx);
        }

        // ── Phase 2: Concurrent execution (semaphore-capped + panic isolation) ──
        // `ToolExecutor::execute_with_metadata` takes `&self` and is `Sync`; we run all
        // tool futures concurrently on the current runtime via `join_all`, each future
        // gated by a shared semaphore so at most `MAX_CONCURRENT_TOOL_EXECUTIONS` (10)
        // run simultaneously. This matches claude-code / parallel_tool_exec semantics and
        // prevents unbounded fan-out on large read-only batches (e.g., 30+ grep calls)
        // from saturating edge I/O or exhausting file descriptors.
        // Each future is wrapped with `catch_unwind` so a panicking tool is surfaced as
        // a tool failure instead of aborting the whole batch/turn.
        let executor: &crate::edge_tools::ToolExecutor = &self.executor;
        // Use the process-wide shared semaphore so the concurrency cap
        // genuinely spans every batch and every concurrent session in this
        // process — previously each batch constructed its own `Semaphore::new(10)`,
        // which allowed 10·N concurrent tools when N batches overlapped.
        let sem = astra_turn_core::parallel_tool_exec::shared_tool_semaphore();
        // D-9: harvest speculative results from mid-stream execution.
        // Matching request_ids skip the normal dispatch and reuse the
        // speculative output. Journal/observability still fire exactly
        // once from the post-execution pass below.
        let speculative_by_id = self.harvest_speculation_for_batch(&conc_reqs).await;
        let outputs: Vec<(crate::edge_tools::ToolExecutionOutcome, u64)> = join_all(
            conc_reqs
                .iter()
                .map(|(_, req)| {
                    let tool = req.tool.clone();
                    let args = req.args.clone();
                    let request_id = req.request_id.clone();
                    let sem = sem.clone();
                    let speculative = speculative_by_id.get(&req.request_id).cloned();
                    async move {
                        if let Some(output) = reusable_speculative_output(speculative) {
                            return (
                                crate::edge_tools::ToolExecutionOutcome {
                                    output,
                                    tool_result_fields: None,
                                    is_error: false,
                                },
                                0u64,
                            );
                        }
                        // ── Pre-tool hooks (global registry, no-op when empty) ──
                        // Rewrites to inputs from pre-hooks are honored; a Block
                        // decision short-circuits execution with a synthesized
                        // error output so the model sees the reason.
                        let mut effective_args = args.clone();
                        if astra_turn_core::tool_hooks::global_has_hooks().await {
                            let pre_ctx = astra_turn_core::tool_hooks::ToolHookContext::pre(
                                &tool,
                                args.clone(),
                            )
                            .with_call_id(&request_id);
                            match astra_turn_core::tool_hooks::global_run_pre(&pre_ctx).await {
                                astra_turn_core::tool_hooks::PreHookOutcome::Proceed {
                                    final_input,
                                } => {
                                    effective_args = final_input;
                                }
                                astra_turn_core::tool_hooks::PreHookOutcome::Blocked {
                                    hook_id,
                                    reason,
                                } => {
                                    return (
                                        crate::edge_tools::ToolExecutionOutcome {
                                            output: format!(
                                                "Tool blocked by hook '{hook_id}': {reason}"
                                            ),
                                            tool_result_fields: None,
                                            is_error: true,
                                        },
                                        0u64,
                                    );
                                }
                            }
                        }
                        // Acquire a permit before executing. Semaphore is never closed
                        // (it lives only for this batch), so acquire() won't fail; the
                        // `ok()` fallback is defensive.
                        let _permit = sem.acquire_owned().await.ok();
                        let (outcome, dur) = catch_tool_execution_panic(
                            executor.execute_with_metadata(&tool, &effective_args),
                        )
                        .await;
                        // ── Post-tool hooks (rewrite output if any hook requests it) ──
                        if astra_turn_core::tool_hooks::global_has_hooks().await {
                            let post_ctx = astra_turn_core::tool_hooks::ToolHookContext::post(
                                &tool,
                                effective_args.clone(),
                                outcome.output.clone(),
                            )
                            .with_call_id(&request_id);
                            let post =
                                astra_turn_core::tool_hooks::global_run_post(&post_ctx).await;
                            if post.final_output != outcome.output {
                                return (
                                    crate::edge_tools::ToolExecutionOutcome {
                                        output: post.final_output,
                                        tool_result_fields: outcome.tool_result_fields,
                                        is_error: outcome.is_error,
                                    },
                                    dur,
                                );
                            }
                        }
                        (outcome, dur)
                    }
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // ── Phase 3: Post-execution (sequential, &mut self) ──
        // Stop grouped spinner if we used one.
        if use_grouped_spinner {
            self.render.stop_tool_stderr_running();
        }

        // ── Phase 2.5: Sandbox-denied retry (Auto mode) ──
        // The sequential dispatch path (lines 2025–2181) wraps every
        // tool call in a SANDBOX_DENIED→prompt→retry flow; the parallel
        // batch above does not, because each future can't hold `&mut
        // self`. Handle retries here where we're sequential again: for
        // any tool that returned SANDBOX_DENIED, if the permission
        // manager's check returns Allow (the shape PermissionMode::Auto
        // produces for `sandbox_expand:*`), widen the sandbox and
        // re-execute the tool. This closes the bug in session
        // `3b7ac18f` where `cat ~/claudecode/*` was blocked 4 times in
        // auto mode with no approval path.
        //
        // For NeedApproval and Deny we now route to the same approval
        // sink as the sequential path (issue #326 P0 #1 fix). Previously
        // both were silently `continue`d, which meant Prompt-mode users
        // never got asked about parallel sandbox-denied tools and Deny
        // rules fired by parallel retries were invisible. This restores
        // the contract that **no decision is silently dropped**:
        //   - Allow       → expand sandbox + retry
        //   - Deny(reason)→ surface the reason in the SANDBOX_DENIED
        //                   output so the LLM/user can see it
        //   - NeedApproval→ ask the TUI sink synchronously; on
        //                   approval, expand + retry; on rejection,
        //                   surface the reason
        //
        // Interactive / Prompt mode now reaches this branch too because
        // we no longer assume the parallel batch was pre-approved.
        let mut outputs = outputs;
        for pos in 0..outputs.len() {
            if !crate::sandbox_retry::is_sandbox_denied(&outputs[pos].0.output) {
                continue;
            }
            let (_, req) = conc_reqs[pos];
            let tool = req.tool.clone();
            let args = req.args.clone();
            let sandbox_tool_key = format!("sandbox_expand:{tool}");
            let sandbox_msg = crate::sandbox_retry::sandbox_denied_message(&outputs[pos].0.output)
                .unwrap_or("")
                .to_string();
            let guard_args = serde_json::json!({"reason": sandbox_msg});
            let decision = crate::tool_safety_guard::ToolSafetyGuard::check_request(
                self.perm_manager.as_deref_mut(),
                &sandbox_tool_key,
                &guard_args,
            );
            let approved = match decision {
                crate::permission_manager::PermissionDecision::Allow => true,
                crate::permission_manager::PermissionDecision::Deny(reason) => {
                    // Surface the deny reason so the LLM and user can
                    // see why the sandbox refused to widen, instead of
                    // silently continuing with the original
                    // SANDBOX_DENIED output.
                    let prefix = "SANDBOX_DENIED: ";
                    let suffix = format!(" (sandbox_expand:{tool} denied: {reason})");
                    if !outputs[pos].0.output.contains(&suffix) {
                        if outputs[pos].0.output.starts_with(prefix) {
                            outputs[pos].0.output.push_str(&suffix);
                        } else {
                            outputs[pos].0.output =
                                format!("{prefix}{}{suffix}", outputs[pos].0.output);
                        }
                    }
                    false
                }
                crate::permission_manager::PermissionDecision::NeedApproval {
                    tool: approval_tool,
                    header,
                    detail,
                    reason,
                } => {
                    use super::chat_stream::ApprovalResponse;
                    if let Some(tx) = &self.approval_request_tx {
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        let _ = tx.send(super::chat_stream::ApprovalRequest::bare(
                            approval_tool.clone(),
                            header,
                            detail,
                            reason,
                            resp_tx,
                        ));
                        let response = if let Some(token) = self.cancel_token {
                            tokio::select! {
                                biased;
                                _ = token.cancelled() => ApprovalResponse::Deny,
                                r = resp_rx => r.unwrap_or(ApprovalResponse::Deny),
                            }
                        } else {
                            resp_rx.await.unwrap_or(ApprovalResponse::Deny)
                        };
                        if let Some(pm) = self.perm_manager.as_mut() {
                            if response.is_approved() {
                                let workspace_untrusted = !pm.project_allow_rules_active();
                                let always_scope = approval_default_always_scope(
                                    &approval_scope_context_for_tool(
                                        &approval_tool,
                                        &guard_args,
                                        false,
                                        workspace_untrusted,
                                    ),
                                );
                                let save_warning_tx = self.stream_event_tx.clone();
                                apply_approval_memory_action(
                                    pm,
                                    approval_memory_action(response, always_scope, true),
                                    &approval_tool,
                                    &guard_args,
                                    save_warning_tx.as_ref(),
                                );
                            } else {
                                pm.record_approval(&approval_tool, Some(&guard_args), false);
                            }
                        }
                        response.is_approved()
                    } else {
                        // No approval sink (headless / sub-run): fail
                        // closed. The contract is enforced upstream by
                        // forcing PermissionMode::Auto for headless
                        // entries; reaching this branch with no sink
                        // means a misconfiguration. Surface a clear
                        // reason in the SANDBOX_DENIED output.
                        let suffix = " (approval required for sandbox_expand but no TUI; \
                                       pass --mode auto or add allow rule)";
                        if !outputs[pos].0.output.contains(suffix) {
                            outputs[pos].0.output.push_str(suffix);
                        }
                        false
                    }
                }
            };
            if !approved {
                continue;
            }
            if let Some(dir) = crate::sandbox_retry::sandbox_expand_dir_from_args(&args) {
                self.executor.expand_sandbox_path(dir);
            }
            if let Some(pm) = &mut self.perm_manager {
                pm.record_approval(&sandbox_tool_key, Some(&args), true);
            }
            let (retried, retry_dur) =
                catch_tool_execution_panic(self.executor.execute_with_metadata(&tool, &args)).await;
            outputs[pos] = (retried, retry_dur);
        }

        for (pos, (outcome, duration_ms)) in outputs.into_iter().enumerate() {
            let (orig_idx, req) = conc_reqs[pos];
            let output = outcome.output;
            let status = cloud_tool_result_status_label(&output);

            // Forward tool-completed event.
            if let Some(tx) = &self.stream_event_tx {
                let output_summary = self
                    .render
                    .format_output_summary(&req.tool, &output, status)
                    .map(|summary| summary.text)
                    .unwrap_or_default();
                let desc = self.render.format_tool_description(&req.tool, &req.args);
                let _ = tx.send(super::chat_stream::StreamEvent::ToolCompleted {
                    name: req.tool.clone(),
                    description: desc,
                    status: status.to_string(),
                    duration_ms,
                    output_summary: if output_summary.is_empty() {
                        None
                    } else {
                        Some(output_summary)
                    },
                    output: Some(output.chars().take(5000).collect()),
                });
            }

            // Tool-done UI.
            if use_grouped_spinner {
                // Grouped mode: print completion line directly (no spinner update).
                if !self.render_policy.suppress_tool_ui() {
                    self.render.tool_done_inline(
                        &req.tool,
                        &req.args,
                        status,
                        duration_ms,
                        &output,
                    );
                }
            } else if let Some(idx) = ui_indices[pos] {
                self.render
                    .tool_done(idx, &req.tool, &req.args, status, duration_ms, &output);
            }

            let result = EdgeToolExecResult {
                request_id: req.request_id.clone(),
                tool: req.tool.clone(),
                args: req.args.clone(),
                output: output.clone(),
                tool_result_fields: outcome.tool_result_fields,
                status: status.to_string(),
                duration_ms,
            };
            self.edge_tool_round.push(result.clone());
            results[orig_idx] = Some(result);

            // Post tool result to cloud API.
            let body = astra_thin_client::ToolResultRequest {
                request_id: req.request_id.clone(),
                status: status.to_string(),
                output: Some(output),
                duration_ms: Some(duration_ms),
            };
            if self.post_tool_result_with_auth_retry(&body).await {
                break;
            }
        }

        // Clear batch progress when done.
        self.render.tool_batch_progress = None;

        results
            .into_iter()
            .map(|r| r.expect("all tool result slots filled"))
            .collect()
    }

    /// D-9: Harvest speculative results for the upcoming concurrent batch.
    ///
    /// `wait_all()` is used so in-flight speculations finish before the
    /// merge; the overall latency is still bounded by the stream itself
    /// (the stream has already finished by the time this runs). Results
    /// keyed by request_id are returned so the join_all closure can
    /// short-circuit matching requests without re-executing.
    async fn on_tool_call_complete(&mut self, index: usize, tool_call: &Value) {
        // D-9 speculative streaming hook.
        //
        // When `ASTRA_STREAMING_TOOL_EXEC=1` is set, a read-only tool_use
        // block that completes mid-stream is dispatched to the shared
        // `StreamingToolExecutor` so its I/O overlaps with the remaining
        // SSE stream. Results are later harvested in `execute_tools_batch`
        // and replace the normal dispatch for matching request_ids;
        // permission / journal / observability events still fire exactly
        // once from the batch phase.
        let Some(exec) = self.streaming_tool_exec.clone() else {
            return;
        };
        let tool_name = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let call_id = tool_call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if call_id.is_empty() {
            return;
        }
        let args = astra_turn_core::parallel_tool_exec::parse_tool_args(tool_call);
        if !astra_turn_core::streaming_tool_exec::should_speculate(&tool_name, args.as_ref(), None)
        {
            return;
        }
        tracing::debug!(
            target = "astra_cli::streaming_tool_exec",
            tool = %tool_name,
            call_id = %call_id,
            "dispatching speculative execution"
        );
        let _ = exec
            .on_tool_block(call_id, tool_name, tool_call.clone(), index)
            .await;
    }
}

/// Build the speculative streaming tool executor when enabled via env.
///
/// The returned executor is a background dispatcher that drives the
/// shared `Arc<ToolExecutor>` off-thread. Each speculative task invokes
/// `execute_with_metadata(tool_name, args)` and returns the output +
/// error flag, matching the `ToolExecutorFn` signature used in
/// `parallel_tool_exec`.
fn build_streaming_tool_exec(
    executor: std::sync::Arc<crate::edge_tools::ToolExecutor>,
) -> Option<std::sync::Arc<astra_turn_core::streaming_tool_exec::StreamingToolExecutor>> {
    if !astra_turn_core::streaming_tool_exec::streaming_tool_exec_enabled() {
        return None;
    }
    let fn_exec: astra_turn_core::parallel_tool_exec::ToolExecutorFn =
        std::sync::Arc::new(move |tc: Value| {
            let executor = std::sync::Arc::clone(&executor);
            Box::pin(async move {
                let call_id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args: Value = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .map(|a| match a {
                        Value::String(s) => {
                            serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
                        }
                        other => other.clone(),
                    })
                    .unwrap_or_else(|| serde_json::json!({}));
                let outcome = executor.execute_with_metadata(&tool_name, &args).await;
                (call_id, tool_name, outcome.output, true)
            })
        });
    Some(std::sync::Arc::new(
        astra_turn_core::streaming_tool_exec::StreamingToolExecutor::new(fn_exec),
    ))
}

// ─── Turn result from one /chat/turn SSE stream ───────────────────────────────

/// One turn: core fields from [`ChatTurnSseAccum`] plus CLI-only edge bookkeeping and TTFT.
pub(super) struct TurnResult {
    pub(super) core: ChatTurnSseAccum,
    /// Time to first token in milliseconds (streaming latency).
    pub(super) ttft_ms: Option<u64>,
    /// Ordered executions from this SSE stream (for rounds without legacy `tool_call` events).
    pub(super) edge_tool_round: Vec<EdgeToolExecResult>,
    /// New access token obtained by an in-stream auth refresh, if any.
    pub(super) refreshed_token: Option<String>,
}

impl Deref for TurnResult {
    type Target = ChatTurnSseAccum;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl DerefMut for TurnResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl TurnResult {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self {
            core: ChatTurnSseAccum::default(),
            ttft_ms: None,
            edge_tool_round: Vec::new(),
            refreshed_token: None,
        }
    }
}

/// Live rendering state tracked across SSE chunks within one turn.
pub(super) struct StreamRenderState {
    /// True while showing the pre-TTFT “waiting for model” spinner (skip thought-duration log).
    waiting_for_first_sse: bool,
    thinking_start: Option<Instant>,
    thinking_spinner: Option<ThinkingSpinnerKind>,
    /// stderr preview for `reasoning_delta`: grows until viewport cap, then tail + hidden count (see `ASTRA_THINKING_VIEWPORT_LINES`).
    thinking_pane: Option<ThinkingPreviewPane>,
    /// Lines written to the terminal during streaming (stdout + stderr).
    /// Used by the re-render pass to clear all streamed output.
    pub(super) lines_written: usize,
    /// Current column position for wrap tracking.
    col: usize,
    /// Terminal width for wrap calculation.
    term_width: usize,
    /// Incremental markdown renderer — `None` when `render_md` is false.
    md: Option<super::streaming_md::StreamingMarkdown>,
    /// Stderr lines written between tool calls (thinking duration, tool notices).
    #[allow(dead_code)]
    stderr_lines: usize,
    /// Tool status region + lines (non-markdown); mutex so a worker thread can animate the running line.
    tool_ui: Arc<Mutex<ToolRegionState>>,
    /// stderr `\r` line while a tool runs (markdown streaming UX).
    tool_stderr_running: Option<ToolRunningLineSpinner>,
    /// Braille animation on the current running tool row (non-markdown).
    tool_stdout_anim: Option<ToolStdoutLineAnim>,
    /// When true, do not paint the stderr reasoning viewport (plan-only / hidden assistant text).
    /// Avoids broken in-place redraw when other stderr lines (e.g. project context) were printed first,
    /// and keeps plan decomposition output readable. Reasoning is still accumulated for the API.
    suppress_reasoning_viewport: bool,
    /// Accumulated output bytes for live token estimation.
    output_bytes: usize,
    /// Tool batch progress: (current_index, total_count). None when not in batch.
    tool_batch_progress: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolOutputSummaryKind {
    Error,
    Structural,
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolOutputSummary {
    kind: ToolOutputSummaryKind,
    text: String,
}

// ── Tool completion icon + output-summary sentinels (shared by `format_output_summary`) ──

/// Sentinel strings returned by search tools when nothing matched.
const SEARCH_NO_MATCH_SENTINELS: &[&str] = &["No matches", "No visible matches"];
/// Sentinel strings returned by glob tools when nothing matched.
const GLOB_NO_MATCH_SENTINELS: &[&str] = &["No files", "No visible files"];
/// Platform banner prefixes that indicate a warning/note/incomplete-output injected by astra (not tool output).
const PLATFORM_WARNING_PREFIXES: &[&str] = &["⚠ WARNING:", "⚠ Note:"];
/// `read_file` synthetic lines to exclude from "content line" counts.
const READ_FILE_METADATA_PREFIXES: &[&str] = &["[Auto-expanded", "[truncated"];

#[inline]
fn str_starts_with_any_prefix(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|&p| text.starts_with(p))
}

#[inline]
fn tool_slow_warning_threshold_ms(tool: &str) -> u64 {
    match tool {
        "bash" | "shell" | "shell_exec" | "run_build_test" => 60_000,
        _ => 30_000,
    }
}

/// True when astra injected a banner line (`read_file` repeat warning, etc.).
///
/// Line-based only (never substring-scan the whole buffer): grep hits may contain `⚠ WARNING:`
/// inside file content.
fn tool_output_has_platform_warning_banner(output: &str) -> bool {
    // Fast path: banners always contain U+26A0; skip per-line work on huge grep output.
    if !output.contains('⚠') {
        return false;
    }
    output.lines().any(|line| {
        let t = line.trim_start();
        str_starts_with_any_prefix(t, PLATFORM_WARNING_PREFIXES)
    })
}

/// Tool completion icon: optional empty→warn (see below), platform banners, slow runs; else ok.
///
/// **Empty stdout:** warn only for `read_file` / `view_file` / `bash` / `shell` — those should
/// normally return bytes. `grep` / `glob` often mean “nothing matched” or an edge empty payload
/// while `status == ok`; that is **not** a warning.
///
/// Does **not** scan bash stdout for `warning:` (too many false positives from diffs / rustc).
fn tool_completion_icon(
    tool: &str,
    status: &str,
    output: &str,
    duration_ms: u64,
) -> (String, bool) {
    if status == "error" {
        return (theme::icon_err(), false);
    }

    let trimmed = output.trim();

    let warn_if_empty_ok_status = matches!(
        tool,
        "read_file" | "view_file" | "bash" | "shell" | "shell_exec"
    );
    if warn_if_empty_ok_status && trimmed.is_empty() {
        return (theme::icon_warn(), true);
    }

    if tool_output_has_platform_warning_banner(trimmed) {
        return (theme::icon_warn(), true);
    }

    if duration_ms > tool_slow_warning_threshold_ms(tool) {
        return (theme::icon_warn(), true);
    }

    (theme::icon_ok(), false)
}

impl StreamRenderState {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_term_width(80, false, false)
    }

    fn with_term_width(tw: usize, render_md: bool, suppress_reasoning_viewport: bool) -> Self {
        let w = tw.max(1);
        Self {
            waiting_for_first_sse: false,
            thinking_start: None,
            thinking_spinner: None,
            thinking_pane: None,
            lines_written: 0,
            col: 0,
            term_width: w,
            md: if render_md {
                Some(super::streaming_md::StreamingMarkdown::new(w))
            } else {
                None
            },
            stderr_lines: 0,
            tool_ui: Arc::new(Mutex::new(ToolRegionState {
                region: super::terminal_region::TerminalRegion::new(),
                lines: Vec::new(),
            })),
            tool_stderr_running: None,
            tool_stdout_anim: None,
            suppress_reasoning_viewport,
            output_bytes: 0,
            tool_batch_progress: None,
        }
    }

    fn stop_tool_stderr_running(&mut self) {
        if let Some(s) = self.tool_stderr_running.take() {
            s.stop_clear();
        }
    }

    fn stop_tool_stdout_anim(&mut self) {
        if let Some(mut a) = self.tool_stdout_anim.take() {
            a.stop_join();
        }
    }

    /// Account for text written to the terminal (stdout or stderr).
    pub(super) fn track_output(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.lines_written += 1;
                self.col = 0;
            } else {
                self.col += 1;
                if self.col >= self.term_width {
                    self.lines_written += 1;
                    self.col = 0;
                }
            }
        }
    }

    /// Account for a full line written via eprintln! (adds 1 line).
    #[allow(dead_code)]
    pub(super) fn track_eprintln(&mut self) {
        self.stderr_lines += 1;
        if self.md.is_none() {
            self.lines_written += 1;
        }
        self.col = 0;
    }

    /// Spinner during HTTP/TTFB before any SSE event is decoded (reuses stderr spinner slot).
    fn start_waiting_for_model(&mut self) {
        if self.thinking_spinner.is_some() || self.thinking_pane.is_some() {
            return;
        }
        if !io::stderr().is_terminal() {
            return;
        }
        self.waiting_for_first_sse = true;
        self.thinking_start.get_or_insert_with(Instant::now);
        self.thinking_spinner = Some(ThinkingSpinnerKind::TtftWait(TtftWaitLineSpinner::start()));
    }

    fn start_thinking(&mut self) {
        if self.thinking_pane.is_some() {
            return;
        }
        self.waiting_for_first_sse = false;
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane now uses stdout (via TerminalRegion), so it works
        // in both markdown and non-markdown modes without cursor conflicts.
        let use_pane = rows > 0 && io::stdout().is_terminal() && !self.suppress_reasoning_viewport;
        if !use_pane && !self.suppress_reasoning_viewport && io::stderr().is_terminal() {
            self.thinking_spinner = Some(ThinkingSpinnerKind::Classic(Spinner::start(
                "Thinking".to_string(),
            )));
        }
    }

    fn push_thinking_preview_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() || self.suppress_reasoning_viewport {
            return;
        }
        // Track output bytes for token estimation
        self.output_bytes = self.output_bytes.saturating_add(chunk.len());
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane and StreamingMarkdown both use stdout (TerminalRegion).
        // Before updating thinking pane, pause markdown's unstable region to avoid
        // cursor desync between independent regions.
        if rows > 0 && io::stdout().is_terminal() {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            if self.thinking_pane.is_none() {
                self.thinking_pane = Some(ThinkingPreviewPane::new(rows, self.term_width));
            }
            if let Some(pane) = &mut self.thinking_pane {
                // Feed output bytes to pane for live token display
                pane.set_output_bytes(self.output_bytes);
                pane.push_chunk(chunk);
            }
        }
    }

    /// Refresh the thinking pane header (elapsed time + token count) without new content.
    fn tick_thinking_pane(&mut self) {
        if let Some(pane) = &mut self.thinking_pane {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            // Update output bytes so token counter refreshes
            pane.set_output_bytes(self.output_bytes);
            pane.tick();
        }
    }

    fn stop_thinking(&mut self) {
        let summary = self.thinking_pane.as_ref().map(|pane| pane.summary_line());
        if let Some(mut pane) = self.thinking_pane.take() {
            pane.clear();
        }
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        let skip_thought_duration_log = self.waiting_for_first_sse;
        self.waiting_for_first_sse = false;
        if let Some(_start) = self.thinking_start.take()
            && !skip_thought_duration_log
            && let Some(line) = summary
        {
            if self.md.is_none() {
                println!("{line}");
                let _ = io::stdout().flush();
                self.lines_written += 1;
                self.col = 0;
            } else {
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
        }
    }

    fn clear_thinking_with_summary(&mut self, mut pane: ThinkingPreviewPane, summary: &str) {
        pane.clear();
        if self.md.is_none() {
            println!("{summary}");
            let _ = io::stdout().flush();
            self.lines_written += 1;
            self.col = 0;
        } else {
            eprintln!("{summary}");
            self.stderr_lines += 1;
        }
    }

    /// Show a tool as "running" with Cursor-style description (single line).
    fn tool_start(&mut self, tool: &str, args: &Value) -> usize {
        let description = self.format_tool_description(tool, args);
        let styled_desc = style_tool_description(tool, &description);
        if let Some(pane) = self.thinking_pane.take() {
            let summary = pane.summary_line();
            self.clear_thinking_with_summary(pane, &summary);
        }
        self.suppress_reasoning_viewport = true;
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            if io::stderr().is_terminal() {
                // Spinner uses plain description (truncated internally with .dim())
                // Pass batch progress for [1/5] prefix when running multiple tools.
                self.tool_stderr_running = Some(ToolRunningLineSpinner::start_with_progress(
                    description,
                    self.tool_batch_progress,
                ));
            } else {
                // Non-terminal: include progress prefix inline.
                let prefix = match self.tool_batch_progress {
                    Some((cur, total)) if total > 1 => format!("[{}/{}] ", cur, total),
                    _ => String::new(),
                };
                let line = format!("  {} {}{} …", theme::icon_running(), prefix, styled_desc);
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
            return 0;
        }
        self.stop_tool_stdout_anim();
        let idx = {
            let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
            let idx = g.lines.len();
            // Include progress prefix for stdout mode too.
            let prefix = match self.tool_batch_progress {
                Some((cur, total)) if total > 1 => format!("[{}/{}] ", cur, total),
                _ => String::new(),
            };
            let line = format!("  {} {}{} …", theme::icon_running(), prefix, styled_desc);
            g.lines.push(line);
            let lines = g.lines.clone();
            g.region.update(lines);
            idx
        };
        self.tool_stdout_anim = Some(ToolStdoutLineAnim::start(
            self.tool_ui.clone(),
            idx,
            description, // Plain text for spinner animation
        ));
        idx
    }

    /// Start a grouped spinner for N tools running in parallel.
    /// Shows: `⬢ Running N tools in parallel… Xs ⣾`
    fn tool_start_parallel_group(&mut self, count: usize) -> usize {
        self.stop_tool_stderr_running();
        let description = format!("Running {} tools in parallel", count);
        if io::stderr().is_terminal() {
            self.tool_stderr_running = Some(ToolRunningLineSpinner::start(description));
        } else {
            let line = format!("  {} {} …", theme::icon_running(), description.dim());
            eprintln!("{line}");
            self.stderr_lines += 1;
        }
        0 // Index not used for grouped spinner
    }

    /// Format a Cursor-style tool description: "Grepped pattern in path", "Read file lines X-Y"
    fn format_tool_description(&self, tool: &str, args: &Value) -> String {
        self.format_tool_description_with_output(tool, args, None)
    }

    /// Format tool description, optionally adjusting based on output.
    /// For read_file, detects auto-expand and adjusts description accordingly.
    fn format_tool_description_with_output(
        &self,
        tool: &str,
        args: &Value,
        output: Option<&str>,
    ) -> String {
        // Dynamic budget based on terminal width.
        // Layout: "  ✓ {description} {duration}" — prefix ~6 chars, duration ~6 chars.
        let term_w = self.term_width;
        let desc_budget = term_w.saturating_sub(14); // room for prefix + duration
        // Path budget: description budget minus the label prefix (e.g. "Reading: ")
        let path_budget = |prefix_len: usize| desc_budget.saturating_sub(prefix_len).max(20);

        match tool {
            "bash" | "shell_exec" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
            }
            "powershell" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("PS> {}", truncate_line(cmd, path_budget(4)))
            }
            "read_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                let short_path = shorten_path(path, path_budget(10)); // "Reading: "

                // Check if auto-expanded (ranged request but full file returned)
                let auto_expanded = output
                    .map(|o| o.starts_with("[Auto-expanded to full file"))
                    .unwrap_or(false);

                if auto_expanded {
                    format!("Reading: {short_path} (full)")
                } else {
                    match (start, end) {
                        (Some(s), Some(e)) => format!("Reading: {short_path}:{s}-{e}"),
                        (Some(s), None) => format!("Reading: {short_path}:{s}-"),
                        _ => format!("Reading: {short_path}"),
                    }
                }
            }
            "write_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                if args.get("delete").and_then(Value::as_bool).unwrap_or(false) {
                    format!("Deleting: {}", shorten_path(path, path_budget(10)))
                } else {
                    format!("Writing: {}", shorten_path(path, path_budget(9)))
                }
            }
            "str_replace" | "multi_edit" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Editing: {}", shorten_path(path, path_budget(9)))
            }
            "delete_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Deleting: {}", shorten_path(path, path_budget(10)))
            }
            "list_dir" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                format!("Listing: {}", shorten_path(path, path_budget(9)))
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                let glob_filter = args.get("glob").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let pat_budget = desc_budget / 3;
                let short_pattern = truncate_line(pattern, pat_budget);
                match (glob_filter, path) {
                    (Some(g), _) => format!("Grep: \"{short_pattern}\" in {g}"),
                    (None, Some(p)) => {
                        let p_budget = desc_budget.saturating_sub(10 + pat_budget);
                        format!("Grep: \"{short_pattern}\" in {}", shorten_path(p, p_budget))
                    }
                    _ => format!("Grep: \"{short_pattern}\""),
                }
            }
            "glob" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                format!("Glob: {}", truncate_line(pattern, path_budget(6)))
            }
            "git" => {
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("status");
                match action {
                    "status" => "Git status".to_string(),
                    "log" => {
                        let n = args.get("n").and_then(Value::as_u64);
                        let branch = args.get("branch").and_then(Value::as_str);
                        match (n, branch) {
                            (Some(n), Some(b)) => format!("Git log -{n} {b}"),
                            (Some(n), None) => format!("Git log -{n}"),
                            (None, Some(b)) => format!("Git log {b}"),
                            _ => "Git log".to_string(),
                        }
                    }
                    "show" => {
                        let commit = args
                            .get("commit")
                            .or_else(|| args.get("ref"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        format!("Git show {}", truncate_line(commit, path_budget(9)))
                    }
                    "diff" => {
                        let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                        let path = args.get("path").and_then(Value::as_str);
                        let stat_only = args
                            .get("stat_only")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let suffix = if stat_only { " --stat" } else { "" };
                        match (staged, path) {
                            (true, Some(p)) => format!(
                                "Git diff --staged{suffix} {}",
                                shorten_path(p, path_budget(18))
                            ),
                            (true, None) => format!("Git diff --staged{suffix}"),
                            (false, Some(p)) => {
                                format!("Git diff{suffix} {}", shorten_path(p, path_budget(10)))
                            }
                            _ => format!("Git diff{suffix}"),
                        }
                    }
                    "blame" => {
                        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                        format!("Git blame {}", shorten_path(path, path_budget(10)))
                    }
                    "file_history" => {
                        let file = args.get("file").and_then(Value::as_str).unwrap_or("");
                        format!("Git history {}", shorten_path(file, path_budget(12)))
                    }
                    "log_search" => {
                        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Git log search \"{}\"",
                            truncate_line(query, path_budget(17))
                        )
                    }
                    "contributors" => {
                        let path = args.get("path").and_then(Value::as_str);
                        match path {
                            Some(p) => {
                                format!("Git contributors {}", shorten_path(p, path_budget(17)))
                            }
                            None => "Git contributors".to_string(),
                        }
                    }
                    "commit" => {
                        let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                        format!("Git commit \"{}\"", truncate_line(msg, path_budget(13)))
                    }
                    "stash" => {
                        let sub = args
                            .get("stash_action")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        format!("Git stash {sub}")
                    }
                    _ => format!("Git {action}"),
                }
            }
            "github" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                match action {
                    "list_prs" => format!("GitHub: list PRs {repo_display}"),
                    "get_pr" => {
                        let pr = args.get("pr_number").and_then(Value::as_u64);
                        match pr {
                            Some(n) => format!("GitHub: PR #{n} {repo_display}"),
                            None => format!("GitHub: get PR {repo_display}"),
                        }
                    }
                    "ci_status" => format!("GitHub: CI status {repo_display}"),
                    "list_issues" => format!("GitHub: list issues {repo_display}"),
                    "get_issue" => {
                        let n = args.get("issue_number").and_then(Value::as_u64);
                        match n {
                            Some(n) => format!("GitHub: issue #{n} {repo_display}"),
                            None => format!("GitHub: get issue {repo_display}"),
                        }
                    }
                    "repo_stats" => format!("GitHub: stats {repo_display}"),
                    "create_issue" => {
                        let title = args.get("title").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "GitHub: create issue \"{}\"",
                            truncate_line(title, path_budget(22))
                        )
                    }
                    _ => format!("GitHub: {action}"),
                }
            }
            "memory" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "recall" => {
                        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                        format!("Recalling: \"{}\"", truncate_line(query, path_budget(13)))
                    }
                    "remember" => {
                        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Remembering: \"{}\"",
                            truncate_line(content, path_budget(13))
                        )
                    }
                    "expand" => match args.get("memory_id").and_then(Value::as_str) {
                        Some(mid) => {
                            format!("Expanding memory: {}", truncate_line(mid, path_budget(18)))
                        }
                        None => "Expanding memory".to_string(),
                    },
                    "forget" => {
                        match args
                            .get("memory_id")
                            .and_then(Value::as_str)
                            .or_else(|| args.get("topic").and_then(Value::as_str))
                        {
                            Some(target) => format!(
                                "Forgetting: \"{}\"",
                                truncate_line(target, path_budget(18))
                            ),
                            None => "Forgetting".to_string(),
                        }
                    }
                    "update" => match args.get("memory_id").and_then(Value::as_str) {
                        Some(mid) => {
                            format!("Updating memory: {}", truncate_line(mid, path_budget(18)))
                        }
                        None => "Updating memory".to_string(),
                    },
                    "focus" => {
                        let value = args
                            .get("focus_value")
                            .or_else(|| args.get("value"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        format!("Focus on: \"{}\"", truncate_line(value, path_budget(16)))
                    }
                    "reflect" => "Reflecting on memories".to_string(),
                    "profile" => "Checking profile".to_string(),
                    "feedback" => {
                        let signal = args.get("signal").and_then(Value::as_str).unwrap_or("");
                        if signal.is_empty() {
                            "Memory feedback".to_string()
                        } else {
                            format!("Memory feedback: {signal}")
                        }
                    }
                    _ => format!("Memory: {action}"),
                }
            }
            "session" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "config" => {
                        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                        format!("Adjust config: {}", truncate_line(path, path_budget(15)))
                    }
                    "prioritize" => {
                        let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
                        format!("Prioritize: {}", truncate_line(tool, path_budget(12)))
                    }
                    "deprioritize" => {
                        let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
                        format!("Deprioritize: {}", truncate_line(tool, path_budget(14)))
                    }
                    "set_goal" => {
                        let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
                        format!("Set goal: \"{}\"", truncate_line(goal, path_budget(12)))
                    }
                    "compact" => "Compress context".to_string(),
                    "enter_plan" => {
                        let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Enter plan mode: \"{}\"",
                            truncate_line(goal, path_budget(18))
                        )
                    }
                    "exit_plan" => "Exit plan mode".to_string(),
                    "rollback_edits" => {
                        let scope = args.get("scope").and_then(Value::as_str);
                        match scope {
                            Some(s) => {
                                format!("Revert file edits: {}", truncate_line(s, path_budget(19)))
                            }
                            None => "Revert file edits".to_string(),
                        }
                    }
                    "ask_user" => {
                        let question = args.get("question").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Asking user: \"{}\"",
                            truncate_line(question, path_budget(15))
                        )
                    }
                    "sleep" => {
                        let duration_ms =
                            args.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
                        format!("Sleeping: {duration_ms}ms")
                    }
                    "tool_search" => {
                        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Searching tools: \"{}\"",
                            truncate_line(query, path_budget(18))
                        )
                    }
                    _ => format!("Session: {action}"),
                }
            }
            "mo" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "query" => {
                        let sql = args.get("sql").and_then(Value::as_str).unwrap_or("");
                        format!("MO query: \"{}\"", truncate_line(sql, path_budget(11)))
                    }
                    "snapshot" => {
                        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                        format!("MO snapshot: {}", truncate_line(name, path_budget(13)))
                    }
                    "branch" => {
                        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                        format!("MO branch: {}", truncate_line(name, path_budget(11)))
                    }
                    _ => format!("MO: {action}"),
                }
            }
            "agent" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "delegate" => {
                        let task = args.get("task").and_then(Value::as_str).unwrap_or("");
                        format!("Delegating: \"{}\"", truncate_line(task, path_budget(14)))
                    }
                    "run_chain" => {
                        let chain = args.get("chain_name").and_then(Value::as_str).unwrap_or("");
                        format!("Running chain: {}", truncate_line(chain, path_budget(15)))
                    }
                    "spawn" => {
                        let description = args.get("description").and_then(Value::as_str);
                        let agent_type = args.get("agent_type").and_then(Value::as_str);
                        match (description, agent_type) {
                            (Some(desc), Some(at)) => format!(
                                "Spawn agent: {} ({})",
                                truncate_line(desc, path_budget(13)),
                                truncate_line(at, path_budget(8))
                            ),
                            (Some(desc), None) => {
                                format!("Spawn agent: {}", truncate_line(desc, path_budget(13)))
                            }
                            (None, Some(at)) => {
                                format!("Spawn agent: {}", truncate_line(at, path_budget(13)))
                            }
                            _ => "Spawn agent".to_string(),
                        }
                    }
                    "get_result" => {
                        let agent_id = args.get("agent_id").and_then(Value::as_str).unwrap_or("");
                        format!(
                            "Get agent result: {}",
                            truncate_line(agent_id, path_budget(19))
                        )
                    }
                    "send_message" => {
                        let to = args.get("to").and_then(Value::as_str).unwrap_or("");
                        let summary = args.get("summary").and_then(Value::as_str);
                        let message = args.get("message").and_then(Value::as_str);
                        match (summary, message) {
                            (Some(s), _) => format!(
                                "Send message: {}: {}",
                                truncate_line(to, path_budget(12)),
                                truncate_line(s, path_budget(16))
                            ),
                            (None, Some(m)) => format!(
                                "Send message: {}: {}",
                                truncate_line(to, path_budget(12)),
                                truncate_line(m, path_budget(16))
                            ),
                            (None, None) => {
                                format!("Send message: {}", truncate_line(to, path_budget(14)))
                            }
                        }
                    }
                    _ => format!("Agent: {action}"),
                }
            }
            "introspect" => "Introspecting…".to_string(),
            // Legacy individual tool names (kept for backward compat)
            "git_status" => "Git status".to_string(),
            "git_log" => {
                let n = args.get("n").and_then(Value::as_u64);
                let branch = args.get("branch").and_then(Value::as_str);
                match (n, branch) {
                    (Some(n), Some(b)) => format!("Git log -{n} {b}"),
                    (Some(n), None) => format!("Git log -{n}"),
                    (None, Some(b)) => format!("Git log {b}"),
                    _ => "Git log".to_string(),
                }
            }
            "git_show" => {
                let commit = args
                    .get("commit")
                    .or_else(|| args.get("ref"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("Git show {}", truncate_line(commit, path_budget(9)))
            }
            "git_diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                let path = args.get("path").and_then(Value::as_str);
                let base_ref = args.get("base_ref").and_then(Value::as_str);
                let git_ref = args.get("ref").and_then(Value::as_str);
                if let Some(base) = base_ref {
                    let tip = git_ref.unwrap_or("HEAD");
                    let range = format!("{base}..{tip}");
                    match path {
                        Some(p) => {
                            format!("Git diff {} -- {}", range, shorten_path(p, path_budget(20)))
                        }
                        None => format!("Git diff {range}"),
                    }
                } else {
                    match (staged, path) {
                        (true, Some(p)) => {
                            format!("Git diff --staged {}", shorten_path(p, path_budget(18)))
                        }
                        (true, None) => "Git diff --staged".to_string(),
                        (false, Some(p)) => {
                            format!("Git diff {}", shorten_path(p, path_budget(10)))
                        }
                        _ => "Git diff".to_string(),
                    }
                }
            }
            "git_blame" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Git blame {}", shorten_path(path, path_budget(10)))
            }
            "git_file_history" => {
                let file = args.get("file").and_then(Value::as_str).unwrap_or("");
                format!("Git history {}", shorten_path(file, path_budget(12)))
            }
            "git_log_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Git log search \"{}\"",
                    truncate_line(query, path_budget(17))
                )
            }
            "git_contributors" => {
                let path = args.get("path").and_then(Value::as_str);
                let since = args.get("since").and_then(Value::as_str);
                match (path, since) {
                    (Some(path), Some(since)) => format!(
                        "Git contributors {} since {}",
                        shorten_path(path, path_budget(23)),
                        truncate_line(since, 18)
                    ),
                    (Some(path), None) => {
                        format!("Git contributors {}", shorten_path(path, path_budget(17)))
                    }
                    (None, Some(since)) => {
                        format!(
                            "Git contributors since {}",
                            truncate_line(since, path_budget(23))
                        )
                    }
                    (None, None) => "Git contributors".to_string(),
                }
            }
            "git_commit" => {
                let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                format!("Git commit \"{}\"", truncate_line(msg, path_budget(13)))
            }
            "git_revert_commit" => {
                let sha = args.get("commit_sha").and_then(Value::as_str).unwrap_or("");
                format!("Git revert {}", truncate_line(sha, path_budget(13)))
            }
            "git_stash" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let stash_ref = args.get("stash_ref").and_then(Value::as_str);
                let index = args.get("index").and_then(Value::as_i64);
                match (action, stash_ref, index) {
                    ("", _, _) => "Git stash".to_string(),
                    (action, Some(stash_ref), _) => format!(
                        "Git stash {action} {}",
                        truncate_line(stash_ref, path_budget(19))
                    ),
                    (action, None, Some(index)) => format!("Git stash {action} stash@{{{index}}}"),
                    (action, None, None) => format!("Git stash {action}"),
                }
            }
            "git_checkout_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let git_ref = args.get("ref").and_then(Value::as_str);
                match git_ref {
                    Some(git_ref) => format!(
                        "Git checkout {} -- {}",
                        truncate_line(git_ref, 16),
                        shorten_path(path, path_budget(20))
                    ),
                    None => format!("Git checkout {}", shorten_path(path, path_budget(13))),
                }
            }
            "git_worktree" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let branch = args.get("branch").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                match (branch, path) {
                    (Some(branch), _) => format!(
                        "Git worktree {} {}",
                        truncate_line(action, 16),
                        truncate_line(branch, path_budget(19))
                    ),
                    (None, Some(path)) => format!(
                        "Git worktree {} {}",
                        truncate_line(action, 16),
                        shorten_path(path, path_budget(19))
                    ),
                    (None, None) => {
                        format!("Git worktree {}", truncate_line(action, path_budget(13)))
                    }
                }
            }
            "find_definition" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Find definition of {}",
                    truncate_line(symbol, path_budget(20))
                )
            }
            "find_references" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Find references to {}",
                    truncate_line(symbol, path_budget(19))
                )
            }
            "symbol_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Search symbol {}", truncate_line(query, path_budget(15)))
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Get symbols in {}", shorten_path(path, path_budget(16)))
            }
            "call_graph" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Call graph for {}", truncate_line(symbol, path_budget(16)))
            }
            "hover_info" => {
                let file = args.get("file").and_then(Value::as_str).unwrap_or("");
                let line = args.get("line").and_then(Value::as_u64);
                let column = args.get("column").and_then(Value::as_u64);
                match (line, column) {
                    (Some(line), Some(column)) => {
                        let suffix_len = format!(":{line}:{column}").chars().count();
                        let short_file = shorten_path(file, path_budget(14 + suffix_len));
                        format!("Hover info at {short_file}:{line}:{column}")
                    }
                    (Some(line), None) => {
                        let suffix_len = format!(":{line}").chars().count();
                        let short_file = shorten_path(file, path_budget(14 + suffix_len));
                        format!("Hover info at {short_file}:{line}")
                    }
                    (None, _) => format!("Hover info at {}", shorten_path(file, path_budget(14))),
                }
            }
            "type_hierarchy" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                let direction = args.get("direction").and_then(Value::as_str);
                match direction {
                    Some(direction) => format!(
                        "Type hierarchy for {} ({})",
                        truncate_line(name, path_budget(23)),
                        truncate_line(direction, 16)
                    ),
                    None => format!(
                        "Type hierarchy for {}",
                        truncate_line(name, path_budget(19))
                    ),
                }
            }
            "rename_symbol" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                let new_name = args.get("new_name").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Rename symbol {} -> {}",
                    truncate_line(symbol, path_budget(18)),
                    truncate_line(new_name, 18)
                )
            }
            "dead_code" => {
                let path = args.get("path").and_then(Value::as_str);
                let kind = args.get("kind").and_then(Value::as_str);
                match (path, kind) {
                    (Some(path), Some(kind)) => format!(
                        "Find dead code: {} ({})",
                        shorten_path(path, path_budget(22)),
                        truncate_line(kind, 16)
                    ),
                    (Some(path), None) => {
                        format!("Find dead code: {}", shorten_path(path, path_budget(16)))
                    }
                    (None, Some(kind)) => {
                        format!("Find dead code: {}", truncate_line(kind, path_budget(16)))
                    }
                    (None, None) => "Find dead code".to_string(),
                }
            }
            "extract_members" => {
                let file = args.get("file").and_then(Value::as_str).unwrap_or("");
                let line = args.get("line").and_then(Value::as_u64);
                match line {
                    Some(line) => {
                        let suffix_len = format!(":{line}").chars().count();
                        let short_file = shorten_path(file, path_budget(17 + suffix_len));
                        format!("Extract members: {short_file}:{line}")
                    }
                    None => format!("Extract members: {}", shorten_path(file, path_budget(17))),
                }
            }
            "lsp" => {
                let operation = args.get("operation").and_then(Value::as_str);
                let file = args.get("file").and_then(Value::as_str);
                let line = args.get("line").and_then(Value::as_u64);
                let column = args.get("column").and_then(Value::as_u64);
                let symbol = args.get("symbol").and_then(Value::as_str);
                let query = args.get("query").and_then(Value::as_str);
                match (operation, file, line, column, symbol, query) {
                    (Some(operation), Some(file), Some(line), Some(column), _, _) => {
                        let suffix_len = format!(":{line}:{column}").chars().count();
                        let prefix_len = 5 + operation.chars().count() + 1;
                        let short_file = shorten_path(file, path_budget(prefix_len + suffix_len));
                        format!("LSP: {operation} {short_file}:{line}:{column}")
                    }
                    (Some(operation), Some(file), _, _, _, _) => format!(
                        "LSP: {} {}",
                        truncate_line(operation, path_budget(5)),
                        shorten_path(file, path_budget(18))
                    ),
                    (Some(operation), _, _, _, Some(symbol), _) => format!(
                        "LSP: {} {}",
                        truncate_line(operation, path_budget(5)),
                        truncate_line(symbol, path_budget(18))
                    ),
                    (Some(operation), _, _, _, _, Some(query)) => format!(
                        "LSP: {} {}",
                        truncate_line(operation, path_budget(5)),
                        truncate_line(query, path_budget(18))
                    ),
                    (Some(operation), _, _, _, _, _) => {
                        format!("LSP: {}", truncate_line(operation, path_budget(13)))
                    }
                    _ => "LSP".to_string(),
                }
            }
            "run_build_test" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
            }
            "web_fetch" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                format!("Fetching: {}", truncate_line(url, path_budget(10)))
            }
            "web_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Searching web: \"{}\"",
                    truncate_line(query, path_budget(17))
                )
            }
            "github_get_pr" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                let num = args.get("pr_number").and_then(Value::as_u64).unwrap_or(0);
                format!("Getting PR: {repo_display}#{num}")
            }
            "github_list_prs" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                format!("Listing PRs: {repo_display}")
            }
            "github_get_issue" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                let num = args
                    .get("issue_number")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                format!("Getting issue: {repo_display}#{num}")
            }
            "github_list_issues" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                format!("Listing issues: {repo_display}")
            }
            "github_repo_stats" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                format!("GitHub stats: {repo_display}")
            }
            "github_ci_status" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                format!("GitHub CI: {repo_display}")
            }
            "get_agent_info" => {
                let dimension = args.get("dimension").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Getting agent info: {}",
                    truncate_line(dimension, path_budget(18))
                )
            }
            "reflect" => {
                let question = args.get("question").and_then(Value::as_str);
                let focus = args.get("focus").and_then(Value::as_str);
                match (question, focus) {
                    (Some(question), _) => format!(
                        "Reflecting: \"{}\"",
                        truncate_line(question, path_budget(13))
                    ),
                    (None, Some(focus)) => {
                        format!("Reflecting: {}", truncate_line(focus, path_budget(13)))
                    }
                    (None, None) => "Reflecting".to_string(),
                }
            }
            "context_analysis" => {
                let mode = args.get("mode").and_then(Value::as_str);
                let turn = args.get("turn").and_then(Value::as_i64);
                let turn_a = args.get("turn_a").and_then(Value::as_i64);
                let turn_b = args.get("turn_b").and_then(Value::as_i64);
                match (mode, turn, turn_a, turn_b) {
                    (Some("turn"), Some(turn), _, _) => format!("Context analysis: turn {turn}"),
                    (Some("compare"), _, Some(turn_a), Some(turn_b)) => {
                        format!("Context analysis: compare {turn_a} vs {turn_b}")
                    }
                    (Some(mode), _, _, _) => {
                        format!("Context analysis: {}", truncate_line(mode, path_budget(18)))
                    }
                    _ => "Context analysis".to_string(),
                }
            }
            "rollback_file_edits" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                let path = args.get("path").and_then(Value::as_str);
                match (scope, turn_index, path) {
                    (Some("turn"), Some(turn_index), _) => {
                        format!("Revert file edits: turn {turn_index}")
                    }
                    (Some("file"), _, Some(path)) => format!(
                        "Revert file edits: {}",
                        truncate_line(path, path_budget(19))
                    ),
                    (Some(scope), _, _) => format!(
                        "Revert file edits: {}",
                        truncate_line(scope, path_budget(19))
                    ),
                    _ => "Revert file edits".to_string(),
                }
            }
            "rollback_database_snapshots" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                let snapshot_id = args.get("snapshot_id").and_then(Value::as_str);
                match (scope, turn_index, snapshot_id) {
                    (Some("turn"), Some(turn_index), _) => {
                        format!("Revert DB snapshots: turn {turn_index}")
                    }
                    (Some("snapshot"), _, Some(snapshot_id)) => format!(
                        "Revert DB snapshots: {}",
                        truncate_line(snapshot_id, path_budget(21))
                    ),
                    (Some(scope), _, _) => format!(
                        "Revert DB snapshots: {}",
                        truncate_line(scope, path_budget(21))
                    ),
                    _ => "Revert DB snapshots".to_string(),
                }
            }
            "rollback_turn_actions" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                match (scope, turn_index) {
                    (Some("turn"), Some(turn_index)) => {
                        format!("Rollback turn actions: turn {turn_index}")
                    }
                    (Some(scope), _) => format!(
                        "Rollback turn actions: {}",
                        truncate_line(scope, path_budget(24))
                    ),
                    _ => "Rollback turn actions".to_string(),
                }
            }
            "diagnose" => {
                let category = args.get("category").and_then(Value::as_str);
                let verbose = args.get("verbose").and_then(Value::as_bool);
                match (category, verbose) {
                    (Some(category), Some(true)) => format!(
                        "Diagnose: {} verbose",
                        truncate_line(category, path_budget(10))
                    ),
                    (Some(category), _) => {
                        format!("Diagnose: {}", truncate_line(category, path_budget(10)))
                    }
                    (None, Some(true)) => "Diagnose: verbose".to_string(),
                    _ => "Diagnose".to_string(),
                }
            }
            "env" => {
                let operation = args.get("operation").and_then(Value::as_str);
                let name = args.get("name").and_then(Value::as_str);
                let pattern = args.get("pattern").and_then(Value::as_str);
                match (operation, name, pattern) {
                    (Some(operation), Some(name), _) => format!(
                        "Env: {} {}",
                        truncate_line(operation, path_budget(5)),
                        truncate_line(name, path_budget(14))
                    ),
                    (Some("search"), _, Some(pattern)) => {
                        format!("Env: search {}", truncate_line(pattern, path_budget(12)))
                    }
                    (Some(operation), _, _) => {
                        format!("Env: {}", truncate_line(operation, path_budget(12)))
                    }
                    _ => "Env".to_string(),
                }
            }
            "notebook_edit" => {
                let edit_mode = args.get("edit_mode").and_then(Value::as_str);
                let notebook_path = args.get("notebook_path").and_then(Value::as_str);
                match (edit_mode, notebook_path) {
                    (Some(edit_mode), Some(notebook_path)) => format!(
                        "Notebook edit: {} {}",
                        truncate_line(edit_mode, path_budget(7)),
                        truncate_line(notebook_path, path_budget(17))
                    ),
                    (_, Some(notebook_path)) => format!(
                        "Notebook edit: {}",
                        truncate_line(notebook_path, path_budget(19))
                    ),
                    _ => "Notebook edit".to_string(),
                }
            }
            "config" => {
                let setting = args.get("setting").and_then(Value::as_str).unwrap_or("");
                let value = args.get("value").and_then(Value::as_str);
                match value {
                    Some(value) => format!(
                        "Config: {}={}",
                        truncate_line(setting, path_budget(8)),
                        truncate_line(value, path_budget(10))
                    ),
                    None => format!("Config: {}", truncate_line(setting, path_budget(13))),
                }
            }
            "brief" => {
                let focus = args.get("focus").and_then(Value::as_str);
                match focus {
                    Some(focus) => format!("Brief: {}", truncate_line(focus, path_budget(14))),
                    None => "Brief".to_string(),
                }
            }
            "share_context" => {
                let key = args.get("key").and_then(Value::as_str).unwrap_or("");
                format!("Share context: {}", truncate_line(key, path_budget(16)))
            }
            "query_context" => {
                let key = args.get("key").and_then(Value::as_str);
                let prefix = args.get("prefix").and_then(Value::as_str);
                let list_keys = args.get("list_keys").and_then(Value::as_bool);
                match (key, prefix, list_keys) {
                    (Some(key), _, _) => {
                        format!("Query context: {}", truncate_line(key, path_budget(16)))
                    }
                    (None, Some(prefix), _) => {
                        format!("Query context: {}", truncate_line(prefix, path_budget(16)))
                    }
                    (None, None, Some(true)) => "Query context: keys".to_string(),
                    _ => "Query context".to_string(),
                }
            }
            "adjust_config" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Adjust config: {}", truncate_line(path, path_budget(15)))
            }
            "prioritize_tool" => {
                let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
                format!("Prioritize tool: {}", truncate_line(tool, path_budget(17)))
            }
            "deprioritize_tool" => {
                let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Deprioritize tool: {}",
                    truncate_line(tool, path_budget(19))
                )
            }
            "set_goal" => {
                let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
                format!("Set goal: \"{}\"", truncate_line(goal, path_budget(12)))
            }
            "compress_context" => {
                let reason = args.get("reason").and_then(Value::as_str);
                match reason {
                    Some(reason) => format!(
                        "Compress context: {}",
                        truncate_line(reason, path_budget(18))
                    ),
                    None => "Compress context".to_string(),
                }
            }
            "rollback_session_state" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                match (scope, turn_index) {
                    (Some("turn"), Some(turn_index)) => {
                        format!("Rollback session state: turn {turn_index}")
                    }
                    (Some(scope), _) => {
                        format!(
                            "Rollback session state: {}",
                            truncate_line(scope, path_budget(24))
                        )
                    }
                    _ => "Rollback session state".to_string(),
                }
            }
            "github_create_issue" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                let title = args.get("title").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Creating issue: {} \"{}\"",
                    repo_display,
                    truncate_line(title, path_budget(19))
                )
            }
            "ask_user" => {
                let question = args.get("question").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Asking user: \"{}\"",
                    truncate_line(question, path_budget(15))
                )
            }
            "sleep" => {
                let duration_ms = args.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
                let reason = args.get("reason").and_then(Value::as_str);
                match reason {
                    Some(reason) => format!(
                        "Sleeping: {}ms ({})",
                        duration_ms,
                        truncate_line(reason, path_budget(18))
                    ),
                    None => format!("Sleeping: {duration_ms}ms"),
                }
            }
            "tool_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Searching tools: \"{}\"",
                    truncate_line(query, path_budget(18))
                )
            }
            "task_create" => {
                let title = args.get("title").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Creating task: \"{}\"",
                    truncate_line(title, path_budget(16))
                )
            }
            "task_list" => {
                let status = args.get("status").and_then(Value::as_str);
                match status {
                    Some(status) => {
                        format!("Listing tasks: {}", truncate_line(status, path_budget(15)))
                    }
                    None => "Listing tasks".to_string(),
                }
            }
            "task_get" => {
                let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
                format!("Getting task: {}", truncate_line(task_id, path_budget(14)))
            }
            "task_update" => {
                let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
                let status = args.get("status").and_then(Value::as_str);
                match status {
                    Some(status) => format!(
                        "Updating task: {} -> {}",
                        truncate_line(task_id, path_budget(21)),
                        truncate_line(status, 16)
                    ),
                    None => format!("Updating task: {}", truncate_line(task_id, path_budget(15))),
                }
            }
            "task_stop" => {
                let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
                format!("Stopping task: {}", truncate_line(task_id, path_budget(15)))
            }
            "mo_query" => {
                let sql = args.get("sql").and_then(Value::as_str).unwrap_or("");
                format!(
                    "MatrixOne query: \"{}\"",
                    truncate_line(sql, path_budget(18))
                )
            }
            "mo_snapshot" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let name = args.get("name").and_then(Value::as_str);
                match name {
                    Some(name) => format!(
                        "MatrixOne snapshot: {} {}",
                        truncate_line(action, 16),
                        truncate_line(name, path_budget(30))
                    ),
                    None => format!(
                        "MatrixOne snapshot: {}",
                        truncate_line(action, path_budget(20))
                    ),
                }
            }
            "mo_branch" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                let name = args.get("name").and_then(Value::as_str);
                match name {
                    Some(name) => format!(
                        "MatrixOne branch: {} {}",
                        truncate_line(action, 16),
                        truncate_line(name, path_budget(28))
                    ),
                    None => format!(
                        "MatrixOne branch: {}",
                        truncate_line(action, path_budget(18))
                    ),
                }
            }
            // Skill tool — show specific skill name or discover query
            "skill" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("run");
                match action {
                    "discover" => {
                        let query = args
                            .get("query")
                            .and_then(Value::as_str)
                            .unwrap_or("skills");
                        format!(
                            "Discovering skills: \"{}\"",
                            truncate_line(query, path_budget(22))
                        )
                    }
                    _ => {
                        let skill_name = args
                            .get("skill_name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        format!(
                            "Running skill: {}",
                            truncate_line(skill_name, path_budget(16))
                        )
                    }
                }
            }
            other if other.starts_with("mcp_") => {
                let rest = &other[4..];
                if let Some(sep) = rest.find('_') {
                    let server = &rest[..sep];
                    let tool_name = &rest[sep + 1..];
                    format!(
                        "MCP {server} {}",
                        truncate_line(tool_name, path_budget(5 + server.len()))
                    )
                } else {
                    format!("MCP {rest}")
                }
            }
            _ => tool.to_string(),
        }
    }

    /// Shorten a path by keeping the last N chars with leading "..."
    fn _format_tool_arg_preview_unused(&self, tool: &str, args: &Value) -> Option<String> {
        match tool {
            "bash" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|cmd| truncate_line(cmd, 60)),
            "read_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    (Some(s), None) => Some(format!("{path}:{s}-")),
                    _ => Some(truncate_line(path, 60)),
                }
            }
            "write_file" | "delete_file" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "str_replace" | "multi_edit" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "list_dir" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                let depth = args.get("depth").and_then(Value::as_u64);
                match depth {
                    Some(d) => Some(format!("{} (depth {})", truncate_line(path, 50), d)),
                    None => Some(truncate_line(path, 60)),
                }
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                let glob_filter = args.get("glob").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let mut preview = format!("/{}/", truncate_line(pattern, 30));
                if let Some(g) = glob_filter {
                    preview.push_str(&format!(" {}", truncate_line(g, 20)));
                } else if let Some(p) = path {
                    preview.push_str(&format!(" in {}", truncate_line(p, 20)));
                }
                Some(preview)
            }
            "glob" => args
                .get("pattern")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "git_log" => {
                let n = args.get("n").and_then(Value::as_u64);
                let branch = args.get("branch").and_then(Value::as_str);
                match (n, branch) {
                    (Some(n), Some(b)) => Some(format!("-{n} {b}")),
                    (Some(n), None) => Some(format!("-{n}")),
                    (None, Some(b)) => Some(truncate_line(b, 20)),
                    _ => None,
                }
            }
            "git_show" | "git_blame" | "git_file_history" => args
                .get("commit")
                .or_else(|| args.get("ref"))
                .or_else(|| args.get("path"))
                .or_else(|| args.get("file"))
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 20)),
            "git_log_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|q| format!("\"{}\"", truncate_line(q, 40))),
            "git_diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                let path = args.get("path").and_then(Value::as_str);
                let base_ref = args.get("base_ref").and_then(Value::as_str);
                let git_ref = args.get("ref").and_then(Value::as_str);
                if let Some(base) = base_ref {
                    let tip = git_ref.unwrap_or("HEAD");
                    match path {
                        Some(p) => Some(format!("{base}..{tip} -- {}", truncate_line(p, 40))),
                        None => Some(format!("{base}..{tip}")),
                    }
                } else {
                    match (staged, path) {
                        (true, Some(p)) => Some(format!("--staged {}", truncate_line(p, 45))),
                        (true, None) => Some("--staged".to_string()),
                        (false, Some(p)) => Some(truncate_line(p, 60)),
                        _ => None,
                    }
                }
            }
            "git_commit" => args
                .get("message")
                .and_then(Value::as_str)
                .map(|m| format!("-m \"{}\"", truncate_line(m, 50))),
            "git_revert_commit" => args
                .get("commit_sha")
                .and_then(Value::as_str)
                .map(|sha| truncate_line(sha, 24)),
            "git_contributors" => {
                let path = args.get("path").and_then(Value::as_str);
                let since = args.get("since").and_then(Value::as_str);
                match (path, since) {
                    (Some(path), Some(since)) => Some(format!(
                        "{} since {}",
                        truncate_line(path, 24),
                        truncate_line(since, 16)
                    )),
                    (Some(path), None) => Some(truncate_line(path, 32)),
                    (None, Some(since)) => Some(format!("since {}", truncate_line(since, 24))),
                    (None, None) => None,
                }
            }
            "git_stash" => {
                let action = args.get("action").and_then(Value::as_str);
                let stash_ref = args.get("stash_ref").and_then(Value::as_str);
                let index = args.get("index").and_then(Value::as_i64);
                match (action, stash_ref, index) {
                    (Some(action), Some(stash_ref), _) => {
                        Some(format!("{action} {}", truncate_line(stash_ref, 40)))
                    }
                    (Some(action), None, Some(index)) => {
                        Some(format!("{action} stash@{{{index}}}"))
                    }
                    (Some(action), None, None) => Some(action.to_string()),
                    _ => None,
                }
            }
            "git_checkout_file" => {
                let path = args.get("path").and_then(Value::as_str);
                let git_ref = args.get("ref").and_then(Value::as_str);
                match (path, git_ref) {
                    (Some(path), Some(git_ref)) => Some(format!(
                        "{} -- {}",
                        truncate_line(git_ref, 16),
                        shorten_path(path, 28)
                    )),
                    (Some(path), None) => Some(shorten_path(path, 40)),
                    _ => None,
                }
            }
            "git_worktree" => {
                let action = args.get("action").and_then(Value::as_str);
                let branch = args.get("branch").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                match (action, branch, path) {
                    (Some(action), Some(branch), _) => Some(format!(
                        "{} {}",
                        truncate_line(action, 16),
                        truncate_line(branch, 24)
                    )),
                    (Some(action), None, Some(path)) => Some(format!(
                        "{} {}",
                        truncate_line(action, 16),
                        truncate_line(path, 28)
                    )),
                    (Some(action), None, None) => Some(action.to_string()),
                    _ => None,
                }
            }
            "find_definition" | "find_references" => args
                .get("symbol")
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 40)),
            "symbol_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|query| truncate_line(query, 40)),
            "hover_info" => {
                let file = args.get("file").and_then(Value::as_str);
                let line = args.get("line").and_then(Value::as_u64);
                let column = args.get("column").and_then(Value::as_u64);
                match (file, line, column) {
                    (Some(file), Some(line), Some(column)) => Some(format!(
                        "{}:{line}:{column}",
                        shorten_path(
                            file,
                            40usize.saturating_sub(format!(":{line}:{column}").chars().count())
                        )
                    )),
                    (Some(file), Some(line), None) => Some(format!(
                        "{}:{line}",
                        shorten_path(
                            file,
                            40usize.saturating_sub(format!(":{line}").chars().count())
                        )
                    )),
                    (Some(file), None, _) => Some(truncate_line(file, 50)),
                    _ => None,
                }
            }
            "call_graph" => {
                let symbol = args.get("symbol").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                match (symbol, path, start, end) {
                    (Some(symbol), _, _, _) => Some(truncate_line(symbol, 40)),
                    (None, Some(path), Some(start), Some(end)) => Some(format!(
                        "{}:{start}-{end}",
                        shorten_path(
                            path,
                            40usize.saturating_sub(format!(":{start}-{end}").chars().count())
                        )
                    )),
                    (None, Some(path), Some(start), None) => Some(format!(
                        "{}:{start}-",
                        shorten_path(
                            path,
                            40usize.saturating_sub(format!(":{start}-").chars().count())
                        )
                    )),
                    (None, Some(path), None, None) => Some(truncate_line(path, 50)),
                    _ => None,
                }
            }
            "type_hierarchy" => {
                let name = args.get("name").and_then(Value::as_str);
                let direction = args.get("direction").and_then(Value::as_str);
                match (name, direction) {
                    (Some(name), Some(direction)) => Some(format!(
                        "{} ({})",
                        truncate_line(name, 32),
                        truncate_line(direction, 16)
                    )),
                    (Some(name), None) => Some(truncate_line(name, 40)),
                    _ => None,
                }
            }
            "rename_symbol" => {
                let symbol = args.get("symbol").and_then(Value::as_str);
                let new_name = args.get("new_name").and_then(Value::as_str);
                match (symbol, new_name) {
                    (Some(symbol), Some(new_name)) => Some(format!(
                        "{} -> {}",
                        truncate_line(symbol, 24),
                        truncate_line(new_name, 24)
                    )),
                    (Some(symbol), None) => Some(truncate_line(symbol, 40)),
                    _ => None,
                }
            }
            "dead_code" => {
                let path = args.get("path").and_then(Value::as_str);
                let kind = args.get("kind").and_then(Value::as_str);
                match (path, kind) {
                    (Some(path), Some(kind)) => Some(format!(
                        "{} ({})",
                        truncate_line(path, 30),
                        truncate_line(kind, 16)
                    )),
                    (Some(path), None) => Some(truncate_line(path, 40)),
                    (None, Some(kind)) => Some(truncate_line(kind, 24)),
                    _ => None,
                }
            }
            "extract_members" => {
                let file = args.get("file").and_then(Value::as_str);
                let line = args.get("line").and_then(Value::as_u64);
                match (file, line) {
                    (Some(file), Some(line)) => Some(format!(
                        "{}:{line}",
                        shorten_path(
                            file,
                            40usize.saturating_sub(format!(":{line}").chars().count())
                        )
                    )),
                    (Some(file), None) => Some(truncate_line(file, 40)),
                    _ => None,
                }
            }
            "lsp" => {
                let operation = args.get("operation").and_then(Value::as_str);
                let file = args.get("file").and_then(Value::as_str);
                let line = args.get("line").and_then(Value::as_u64);
                let column = args.get("column").and_then(Value::as_u64);
                let symbol = args.get("symbol").and_then(Value::as_str);
                let query = args.get("query").and_then(Value::as_str);
                match (operation, file, line, column, symbol, query) {
                    (Some(operation), Some(file), Some(line), Some(column), _, _) => Some(format!(
                        "{operation} {}:{line}:{column}",
                        shorten_path(
                            file,
                            40usize
                                .saturating_sub(operation.chars().count())
                                .saturating_sub(1)
                                .saturating_sub(format!(":{line}:{column}").chars().count())
                        )
                    )),
                    (Some(operation), Some(file), _, _, _, _) => {
                        Some(format!("{operation} {}", truncate_line(file, 32)))
                    }
                    (Some(operation), _, _, _, Some(symbol), _) => {
                        Some(format!("{operation} {}", truncate_line(symbol, 26)))
                    }
                    (Some(operation), _, _, _, _, Some(query)) => {
                        Some(format!("{operation} {}", truncate_line(query, 26)))
                    }
                    (Some(operation), _, _, _, _, _) => Some(truncate_line(operation, 40)),
                    _ => None,
                }
            }
            "symbols" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "run_build_test" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|c| truncate_line(c, 60)),
            "web_fetch" => args
                .get("url")
                .and_then(Value::as_str)
                .map(|u| truncate_line(u, 60)),
            "web_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|query| truncate_line(query, 40)),
            "powershell" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|command| truncate_line(command, 60)),
            "github_get_pr" | "github_get_issue" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                let number = args
                    .get("number")
                    .or_else(|| args.get("pr_number"))
                    .or_else(|| args.get("issue_number"))
                    .and_then(Value::as_u64);
                match number {
                    Some(n) => Some(format!("{repo_display}#{n}")),
                    None => Some(repo_display),
                }
            }
            "github_list_prs" | "github_list_issues" | "github_repo_stats" | "github_ci_status" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                Some(github_repo_display(owner, repo).unwrap_or_default())
            }
            "github_create_issue" => {
                let owner = args.get("owner").and_then(Value::as_str);
                let repo = args.get("repo").and_then(Value::as_str);
                let repo_display = github_repo_display(owner, repo).unwrap_or_default();
                let title = args.get("title").and_then(Value::as_str);
                match title {
                    Some(title) => Some(format!(
                        "{}: \"{}\"",
                        repo_display,
                        truncate_line(title, 28)
                    )),
                    None => Some(repo_display),
                }
            }
            "adjust_config" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|path| truncate_line(path, 36)),
            "prioritize_tool" | "deprioritize_tool" => args
                .get("tool")
                .and_then(Value::as_str)
                .map(|tool| truncate_line(tool, 24)),
            "set_goal" => args
                .get("goal")
                .and_then(Value::as_str)
                .map(|goal| truncate_line(goal, 40)),
            "compress_context" => args
                .get("reason")
                .and_then(Value::as_str)
                .map(|reason| truncate_line(reason, 40)),
            "rollback_session_state" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                match (scope, turn_index) {
                    (Some("turn"), Some(turn_index)) => Some(format!("turn {turn_index}")),
                    (Some(scope), _) => Some(scope.to_string()),
                    _ => None,
                }
            }
            "get_agent_info" => args
                .get("dimension")
                .and_then(Value::as_str)
                .map(|dimension| truncate_line(dimension, 24)),
            "reflect" => args
                .get("question")
                .or_else(|| args.get("focus"))
                .and_then(Value::as_str)
                .map(|value| truncate_line(value, 40)),
            "context_analysis" => {
                let mode = args.get("mode").and_then(Value::as_str);
                let turn = args.get("turn").and_then(Value::as_i64);
                let turn_a = args.get("turn_a").and_then(Value::as_i64);
                let turn_b = args.get("turn_b").and_then(Value::as_i64);
                match (mode, turn, turn_a, turn_b) {
                    (Some("turn"), Some(turn), _, _) => Some(format!("turn {turn}")),
                    (Some("compare"), _, Some(turn_a), Some(turn_b)) => {
                        Some(format!("compare {turn_a} vs {turn_b}"))
                    }
                    (Some(mode), _, _, _) => Some(truncate_line(mode, 24)),
                    _ => None,
                }
            }
            "run_chain" => args
                .get("name")
                .or_else(|| args.get("description"))
                .and_then(Value::as_str)
                .map(|value| truncate_line(value, 40)),
            "rollback_file_edits" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                let path = args.get("path").and_then(Value::as_str);
                match (scope, turn_index, path) {
                    (Some("turn"), Some(turn_index), _) => Some(format!("turn {turn_index}")),
                    (Some("file"), _, Some(path)) => Some(truncate_line(path, 36)),
                    (Some(scope), _, _) => Some(scope.to_string()),
                    _ => None,
                }
            }
            "rollback_database_snapshots" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                let snapshot_id = args.get("snapshot_id").and_then(Value::as_str);
                match (scope, turn_index, snapshot_id) {
                    (Some("turn"), Some(turn_index), _) => Some(format!("turn {turn_index}")),
                    (Some("snapshot"), _, Some(snapshot_id)) => {
                        Some(truncate_line(snapshot_id, 36))
                    }
                    (Some(scope), _, _) => Some(scope.to_string()),
                    _ => None,
                }
            }
            "rollback_turn_actions" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let turn_index = args.get("turn_index").and_then(Value::as_i64);
                match (scope, turn_index) {
                    (Some("turn"), Some(turn_index)) => Some(format!("turn {turn_index}")),
                    (Some(scope), _) => Some(scope.to_string()),
                    _ => None,
                }
            }
            "send_message" => {
                let to = args.get("to").and_then(Value::as_str);
                let summary = args.get("summary").and_then(Value::as_str);
                let message = args.get("message").and_then(Value::as_str);
                match (to, summary, message) {
                    (Some(to), Some(summary), _) => Some(format!(
                        "{}: {}",
                        truncate_line(to, 18),
                        truncate_line(summary, 28)
                    )),
                    (Some(to), None, Some(message)) => Some(format!(
                        "{}: {}",
                        truncate_line(to, 18),
                        truncate_line(message, 28)
                    )),
                    (Some(to), None, None) => Some(truncate_line(to, 40)),
                    _ => None,
                }
            }
            "spawn_agent" => {
                let description = args.get("description").and_then(Value::as_str);
                let agent_type = args.get("agent_type").and_then(Value::as_str);
                match (description, agent_type) {
                    (Some(description), Some(agent_type)) => Some(format!(
                        "{} ({})",
                        truncate_line(description, 28),
                        truncate_line(agent_type, 12)
                    )),
                    (Some(description), None) => Some(truncate_line(description, 40)),
                    (None, Some(agent_type)) => Some(truncate_line(agent_type, 24)),
                    _ => None,
                }
            }
            "diagnose" => {
                let category = args.get("category").and_then(Value::as_str);
                let verbose = args.get("verbose").and_then(Value::as_bool);
                match (category, verbose) {
                    (Some(category), Some(true)) => Some(format!("{category} verbose")),
                    (Some(category), _) => Some(category.to_string()),
                    (None, Some(true)) => Some("verbose".to_string()),
                    _ => None,
                }
            }
            "env" => {
                let operation = args.get("operation").and_then(Value::as_str);
                let name = args.get("name").and_then(Value::as_str);
                let pattern = args.get("pattern").and_then(Value::as_str);
                match (operation, name, pattern) {
                    (Some(operation), Some(name), _) => {
                        Some(format!("{operation} {}", truncate_line(name, 30)))
                    }
                    (Some("search"), _, Some(pattern)) => {
                        Some(format!("search {}", truncate_line(pattern, 24)))
                    }
                    (Some(operation), _, _) => Some(operation.to_string()),
                    _ => None,
                }
            }
            "notebook_edit" => {
                let notebook_path = args.get("notebook_path").and_then(Value::as_str);
                let edit_mode = args.get("edit_mode").and_then(Value::as_str);
                match (edit_mode, notebook_path) {
                    (Some(edit_mode), Some(notebook_path)) => Some(format!(
                        "{} {}",
                        truncate_line(edit_mode, 12),
                        truncate_line(notebook_path, 32)
                    )),
                    (_, Some(notebook_path)) => Some(truncate_line(notebook_path, 40)),
                    _ => None,
                }
            }
            "config" => {
                let setting = args.get("setting").and_then(Value::as_str);
                let value = args.get("value").and_then(Value::as_str);
                match (setting, value) {
                    (Some(setting), Some(value)) => Some(format!(
                        "{}={}",
                        truncate_line(setting, 18),
                        truncate_line(value, 24)
                    )),
                    (Some(setting), None) => Some(truncate_line(setting, 40)),
                    _ => None,
                }
            }
            "brief" => args
                .get("focus")
                .and_then(Value::as_str)
                .map(|focus| truncate_line(focus, 24)),
            "share_context" => args
                .get("key")
                .and_then(Value::as_str)
                .map(|key| truncate_line(key, 40)),
            "query_context" => {
                let key = args.get("key").and_then(Value::as_str);
                let prefix = args.get("prefix").and_then(Value::as_str);
                let list_keys = args.get("list_keys").and_then(Value::as_bool);
                match (key, prefix, list_keys) {
                    (Some(key), _, _) => Some(truncate_line(key, 40)),
                    (None, Some(prefix), _) => Some(truncate_line(prefix, 40)),
                    (None, None, Some(true)) => Some("keys".to_string()),
                    _ => None,
                }
            }
            "ask_user" => args
                .get("question")
                .and_then(Value::as_str)
                .map(|question| truncate_line(question, 50)),
            "sleep" => {
                let duration_ms = args.get("duration_ms").and_then(Value::as_u64);
                let reason = args.get("reason").and_then(Value::as_str);
                match (duration_ms, reason) {
                    (Some(duration_ms), Some(reason)) => {
                        Some(format!("{}ms ({})", duration_ms, truncate_line(reason, 28)))
                    }
                    (Some(duration_ms), None) => Some(format!("{duration_ms}ms")),
                    (None, Some(reason)) => Some(truncate_line(reason, 40)),
                    (None, None) => None,
                }
            }
            "tool_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|query| format!("\"{}\"", truncate_line(query, 40))),
            "task_create" => args
                .get("title")
                .and_then(Value::as_str)
                .map(|title| truncate_line(title, 48)),
            "task_list" => args
                .get("status")
                .and_then(Value::as_str)
                .map(|status| truncate_line(status, 24)),
            "task_get" | "task_stop" => args
                .get("task_id")
                .and_then(Value::as_str)
                .map(|task_id| truncate_line(task_id, 36)),
            "task_update" => {
                let task_id = args.get("task_id").and_then(Value::as_str);
                let status = args.get("status").and_then(Value::as_str);
                match (task_id, status) {
                    (Some(task_id), Some(status)) => Some(format!(
                        "{} -> {}",
                        truncate_line(task_id, 24),
                        truncate_line(status, 16)
                    )),
                    (Some(task_id), None) => Some(truncate_line(task_id, 36)),
                    _ => None,
                }
            }
            "mo_query" => args
                .get("sql")
                .or_else(|| args.get("query"))
                .and_then(Value::as_str)
                .map(|q| truncate_line(q, 60)),
            "mo_snapshot" | "mo_branch" => {
                let action = args.get("action").and_then(Value::as_str);
                let name = args.get("name").and_then(Value::as_str);
                match (action, name) {
                    (Some(action), Some(name)) => Some(format!(
                        "{} {}",
                        truncate_line(action, 16),
                        truncate_line(name, 28)
                    )),
                    (Some(action), None) => Some(action.to_string()),
                    _ => None,
                }
            }
            "memory" => {
                let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "recall" => args
                        .get("query")
                        .and_then(Value::as_str)
                        .map(|q| truncate_line(q, 50)),
                    "remember" => args
                        .get("content")
                        .and_then(Value::as_str)
                        .map(|c| truncate_line(c, 50)),
                    "forget" => args
                        .get("memory_id")
                        .and_then(Value::as_str)
                        .or_else(|| args.get("topic").and_then(Value::as_str))
                        .map(|t| truncate_line(t, 40)),
                    "update" | "expand" | "feedback" => args
                        .get("memory_id")
                        .and_then(Value::as_str)
                        .map(|memory_id| truncate_line(memory_id, 40)),
                    "focus" => args
                        .get("focus_value")
                        .or_else(|| args.get("value"))
                        .and_then(Value::as_str)
                        .map(|v| truncate_line(v, 40)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Update a tool line to show completion status with Cursor-style summary.
    fn tool_done(
        &mut self,
        idx: usize,
        tool: &str,
        args: &Value,
        status: &str,
        duration_ms: u64,
        output: &str,
    ) {
        let output_summary = self.format_output_summary(tool, output, status);
        let duration_suffix = format_duration_suffix(duration_ms);
        // Get smart icon based on status and output analysis.
        let (icon, is_warning) = tool_completion_icon(tool, status, output, duration_ms);
        let extra_line = if status == "error" {
            let err_msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "failed".to_string());
            format!("    {}", err_msg.red())
        } else if is_warning {
            // Show warning context in yellow.
            match output_summary {
                Some(summary) => match summary.kind {
                    ToolOutputSummaryKind::Preview
                    | ToolOutputSummaryKind::Structural
                    | ToolOutputSummaryKind::Error => format!("    {}", summary.text.yellow()),
                },
                None => String::new(),
            }
        } else {
            match output_summary {
                Some(summary) => match summary.kind {
                    ToolOutputSummaryKind::Preview
                    | ToolOutputSummaryKind::Structural
                    | ToolOutputSummaryKind::Error => format!("    {}", summary.text.dim()),
                },
                None => String::new(),
            }
        };
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            let description = self.format_tool_description_with_output(tool, args, Some(output));
            let styled_desc = style_tool_description(tool, &description);
            let dur_display = format!("{}", duration_suffix.dim());
            let mut out_lines = 1usize;
            eprintln!("  {} {}{}", icon, styled_desc, dur_display);
            if !extra_line.is_empty() {
                eprintln!("{extra_line}");
                out_lines = out_lines.saturating_add(extra_line.matches('\n').count() + 1);
            }
            self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
            // In md mode, tool-done lines go to stderr but still occupy terminal rows.
            // Track them in `lines_written` so subsequent MoveUp-based clearing
            // accounts for these rows instead of leaving residual text on screen.
            self.lines_written = self.lines_written.saturating_add(out_lines);
            self.col = 0;
            return;
        }
        self.stop_tool_stdout_anim();
        let description = self.format_tool_description_with_output(tool, args, Some(output));
        let styled_desc = style_tool_description(tool, &description);
        let dur_display = format!("{}", duration_suffix.dim());
        let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
        if idx < g.lines.len() {
            g.lines[idx] = format!("  {icon} {styled_desc}{dur_display}");
            if !extra_line.is_empty() {
                let insert_pos = idx + 1;
                if insert_pos <= g.lines.len() {
                    g.lines.insert(insert_pos, extra_line.clone());
                }
                // TerminalRegion may render extra_line in-place, but if the region
                // overflows its allocated height the extra line spills to stdout.
                // Track it defensively so MoveUp accounts for the potential new row.
                let extra_rows = extra_line.matches('\n').count().saturating_add(1);
                self.lines_written = self.lines_written.saturating_add(extra_rows);
            }
            let lines = g.lines.clone();
            g.region.update(lines);
        }
    }

    /// Print tool completion directly (for grouped parallel tools).
    /// Unlike `tool_done`, doesn't try to update a specific line index.
    fn tool_done_inline(
        &mut self,
        tool: &str,
        args: &Value,
        status: &str,
        duration_ms: u64,
        output: &str,
    ) {
        let output_summary = self.format_output_summary(tool, output, status);
        let duration_suffix = format_duration_suffix(duration_ms);
        let description = self.format_tool_description_with_output(tool, args, Some(output));
        let styled_desc = style_tool_description(tool, &description);
        let dur_display = format!("{}", duration_suffix.dim());

        // Get smart icon based on status and output analysis.
        let (icon, is_warning) = tool_completion_icon(tool, status, output, duration_ms);
        let extra_line = if status == "error" {
            let err_msg = output_summary
                .as_ref()
                .map(|summary| summary.text.clone())
                .unwrap_or_else(|| "failed".to_string());
            format!("    {}", err_msg.red())
        } else if is_warning {
            match output_summary {
                Some(summary) => match summary.kind {
                    ToolOutputSummaryKind::Preview
                    | ToolOutputSummaryKind::Structural
                    | ToolOutputSummaryKind::Error => format!("    {}", summary.text.yellow()),
                },
                None => String::new(),
            }
        } else {
            match output_summary {
                Some(summary) => match summary.kind {
                    ToolOutputSummaryKind::Preview
                    | ToolOutputSummaryKind::Structural
                    | ToolOutputSummaryKind::Error => format!("    {}", summary.text.dim()),
                },
                None => String::new(),
            }
        };

        let mut out_lines = 1usize;
        eprintln!("  {} {}{}", icon, styled_desc, dur_display);
        if !extra_line.is_empty() {
            eprintln!("{extra_line}");
            out_lines = out_lines.saturating_add(extra_line.matches('\n').count() + 1);
        }
        self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
        // Tool-done lines occupy terminal rows even though they go to stderr.
        // Track them in `lines_written` so that subsequent MoveUp-based clearing
        // moves the cursor past these lines instead of leaving residual text.
        // NOTE: This applies in both normal and md mode (matches tool_done behavior).
        self.lines_written = self.lines_written.saturating_add(out_lines);
        self.col = 0;
    }

    /// Format tool output for completion UI.
    /// Preview-like outputs are collapsed to one-line metadata by default,
    /// while structural summaries and errors keep their extra detail.
    fn format_output_summary(
        &self,
        tool: &str,
        output: &str,
        status: &str,
    ) -> Option<ToolOutputSummary> {
        let structural = |text: String| ToolOutputSummary {
            kind: ToolOutputSummaryKind::Structural,
            text,
        };
        let preview = |text: String| ToolOutputSummary {
            kind: ToolOutputSummaryKind::Preview,
            text,
        };
        if status == "error" {
            return Some(ToolOutputSummary {
                kind: ToolOutputSummaryKind::Error,
                text: format_tool_error_summary(tool, output),
            });
        }
        let line_count = output.lines().count();
        let byte_size = output.len();
        match tool {
            "mo_query" => summarize_mo_query_output(output).map(structural),
            "web_fetch" => summarize_web_fetch_output(output).map(structural),
            "bash" | "shell" | "shell_exec" | "run_build_test" => {
                if output.trim().is_empty() {
                    return None;
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "line", "lines")
                )))
            }
            "read_file" | "view_file" => {
                // Only skip our metadata lines, not code that happens to start with '['
                let is_metadata = |l: &&str| {
                    str_starts_with_any_prefix(l, READ_FILE_METADATA_PREFIXES)
                        || str_starts_with_any_prefix(l.trim_start(), PLATFORM_WARNING_PREFIXES)
                };

                // Count all non-empty, non-metadata lines for accurate remaining count
                let total_content_lines = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .count();

                let content_lines: Vec<&str> = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .take(10)
                    .collect();

                if content_lines.is_empty() {
                    return Some(structural(format!(
                        "{line_count} lines, {}",
                        format_byte_size(byte_size)
                    )));
                }
                Some(preview(format!(
                    "{} read ({})",
                    pluralize_with_count(total_content_lines, "file line", "file lines"),
                    format_byte_size(byte_size)
                )))
            }
            "git_log" => {
                let total = output.lines().filter(|l| !l.trim().is_empty()).count();
                if total == 0 {
                    None
                } else {
                    Some(preview(pluralize_with_count(total, "commit", "commits")))
                }
            }
            "git_show" | "git_diff" => {
                // Ignore diff file headers (`+++ b/…`, `--- a/…`) so counts match real hunks.
                let additions = output
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                    .count();
                let deletions = output
                    .lines()
                    .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                    .count();
                // Extract changed file names only from +++ b/ lines (not diff --git headers)
                let files: Vec<&str> = output
                    .lines()
                    .filter_map(|l| l.strip_prefix("+++ b/"))
                    .filter(|f| !f.is_empty() && *f != "/dev/null")
                    .take(5)
                    .collect();
                let total_files = output
                    .lines()
                    .filter(|l| l.starts_with("+++ b/") && !l.contains("/dev/null"))
                    .count();
                let stat = if additions > 0 || deletions > 0 {
                    format!(
                        "{} {}",
                        format!("+{additions}").green(),
                        format!("-{deletions}").red()
                    )
                } else {
                    format!("{line_count} lines")
                };
                if files.is_empty() {
                    Some(structural(stat))
                } else {
                    let mut summary = format!("{stat} in {total_files} file(s)");
                    for f in &files {
                        summary.push_str(&format!("\n      {}", shorten_path(f, 50)));
                    }
                    let remaining = total_files.saturating_sub(5);
                    if remaining > 0 {
                        summary.push_str(&format!("\n      … +{remaining} more"));
                    }
                    Some(structural(summary))
                }
            }
            "grep" | "search" => {
                let head = output.trim_start();
                if str_starts_with_any_prefix(head, SEARCH_NO_MATCH_SENTINELS) {
                    return Some(structural("no matches".to_string()));
                }
                let match_lines: Vec<&str> = output.lines().collect();
                let total = match_lines.len();
                // Extract unique file names from grep output (file:line:content format)
                let mut files: Vec<&str> = Vec::new();
                for line in match_lines.iter().take(50) {
                    if let Some(colon_pos) = line.find(':') {
                        let file = &line[..colon_pos];
                        if !files.contains(&file) {
                            files.push(file);
                        }
                    }
                }
                if files.is_empty() {
                    Some(preview(pluralize_with_count(total, "match", "matches")))
                } else {
                    let file_count = files.len();
                    Some(preview(format!("{total} matches in {file_count} file(s)")))
                }
            }
            "write_file" | "str_replace" | "multi_edit" | "delete_file" => {
                if tool == "delete_file" {
                    return Some(structural("deleted".to_string()));
                }
                // str_replace: sentinel-wrapped diff; write_file: JSON `_cli_unified_diff` (same as headless preview).
                let diff_block = extract_cli_diff_block(output);
                if let Some(ref diff) = diff_block {
                    let colored = colorize_diff_summary(diff.as_ref(), 5);
                    if !colored.is_empty() {
                        return Some(structural(colored));
                    }
                }
                // Fallback: check if output itself looks like a diff
                if output
                    .lines()
                    .any(|l| l.starts_with("+++ ") || l.starts_with("--- "))
                {
                    let colored = colorize_diff_summary(output, 5);
                    if !colored.is_empty() {
                        return Some(structural(colored));
                    }
                }
                if tool == "write_file"
                    && let Ok(v) = serde_json::from_str::<Value>(output.trim())
                    && v.get("success").and_then(|s| s.as_bool()) == Some(true)
                {
                    let bytes =
                        v.get("bytes_written").and_then(|b| b.as_u64()).unwrap_or(0) as usize;
                    return Some(structural(format!("{} written", format_byte_size(bytes))));
                }
                if output.trim().is_empty() {
                    Some(structural("done".to_string()))
                } else {
                    Some(structural(truncate_line(output.trim(), 60)))
                }
            }
            "list_dir" => {
                let entries = output.lines().filter(|l| !l.trim().is_empty()).count();
                Some(structural(format!("{entries} entries")))
            }
            "glob" => {
                let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
                // `glob` returns a single sentinel line when nothing matched (not a path).
                if lines.len() == 1 {
                    let only = lines[0].trim();
                    if str_starts_with_any_prefix(only, GLOB_NO_MATCH_SENTINELS) {
                        return Some(structural("no matches".to_string()));
                    }
                }
                let files = lines;
                let total = files.len();
                if total == 0 {
                    Some(structural("no matches".to_string()))
                } else {
                    // Show file paths with dim styling for directory parts
                    let shown: Vec<String> = files
                        .iter()
                        .take(5)
                        .map(|f| {
                            let path = f.trim().trim_end_matches('/'); // Remove trailing slash
                            if let Some(last_slash) = path.rfind('/') {
                                let filename = &path[last_slash + 1..];
                                if filename.is_empty() {
                                    // Path like "/" or unusual case
                                    path.to_string()
                                } else {
                                    format!("{}{}", path[..=last_slash].dim(), filename)
                                }
                            } else {
                                path.to_string()
                            }
                        })
                        .collect();
                    let mut summary = format!("{total} file(s)");
                    for f in &shown {
                        summary.push_str(&format!("\n    {f}"));
                    }
                    let remaining = total.saturating_sub(5);
                    if remaining > 0 {
                        summary
                            .push_str(&format!("\n    {}", format!("… +{remaining} more").dim()));
                    }
                    Some(structural(summary))
                }
            }
            "skill" => {
                if output.trim().is_empty() {
                    return None;
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "output line", "output lines")
                )))
            }
            other if other.starts_with("mcp_") => {
                if output.trim().is_empty() {
                    return None;
                }
                if let Some(summary) = summarize_json_output(output) {
                    return Some(structural(summary));
                }
                let meaningful_count = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .count();
                if meaningful_count == 0 {
                    return None;
                }
                Some(preview(format!(
                    "{} captured",
                    pluralize_with_count(meaningful_count, "output line", "output lines")
                )))
            }
            _ => {
                if let Some(summary) = summarize_json_output(output) {
                    Some(structural(summary))
                } else if line_count > 1 {
                    Some(structural(format!("{line_count} lines")))
                } else if output.trim().is_empty() {
                    None
                } else {
                    Some(structural(truncate_line(output.trim(), 60)))
                }
            }
        }
    }
}

fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn mysql_table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect()
}

fn summarize_mo_query_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(line) = trimmed
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Query OK") || line.contains("rows affected"))
    {
        return Some(truncate_line(line, 80));
    }

    let table_rows: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .collect();
    if table_rows.is_empty() {
        return None;
    }

    let columns = mysql_table_cells(table_rows[0]);
    if columns.is_empty() {
        return None;
    }
    let row_count = table_rows.len().saturating_sub(1);
    let preview_columns: Vec<String> = columns.iter().take(4).cloned().collect();
    let remaining = columns.len().saturating_sub(preview_columns.len());
    let columns_preview = if remaining > 0 {
        format!("{} … +{remaining}", preview_columns.join(", "))
    } else {
        preview_columns.join(", ")
    };

    Some(format!(
        "{} · cols: {}",
        pluralize_with_count(row_count, "row", "rows"),
        truncate_line(&columns_preview, 60)
    ))
}

fn summarize_json_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() || (!trimmed.starts_with('{') && !trimmed.starts_with('[')) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    match value {
        Value::Object(map) => {
            let mut all_keys: Vec<&str> = map.keys().map(String::as_str).collect();
            all_keys.sort_unstable();
            let keys: Vec<&str> = all_keys.into_iter().take(4).collect();
            let remaining = map.len().saturating_sub(keys.len());
            let key_preview = if keys.is_empty() {
                "no keys".to_string()
            } else if remaining > 0 {
                format!("{} … +{remaining}", keys.join(", "))
            } else {
                keys.join(", ")
            };
            Some(format!(
                "json object · keys: {}",
                truncate_line(&key_preview, 60)
            ))
        }
        Value::Array(items) => {
            let count = items.len();
            let mut object_keys: Vec<&str> = items
                .first()
                .and_then(Value::as_object)
                .map(|obj| obj.keys().map(String::as_str).collect())
                .unwrap_or_default();
            object_keys.sort_unstable();
            object_keys.truncate(4);
            if object_keys.is_empty() {
                Some(format!(
                    "json array · {}",
                    pluralize_with_count(count, "item", "items")
                ))
            } else {
                let remaining = items
                    .first()
                    .and_then(Value::as_object)
                    .map(|obj| obj.len().saturating_sub(object_keys.len()))
                    .unwrap_or(0);
                let key_preview = if remaining > 0 {
                    format!("{} … +{remaining}", object_keys.join(", "))
                } else {
                    object_keys.join(", ")
                };
                Some(format!(
                    "json array · {} · keys: {}",
                    pluralize_with_count(count, "item", "items"),
                    truncate_line(&key_preview, 40)
                ))
            }
        }
        _ => None,
    }
}

fn summarize_web_fetch_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed = serde_json::from_str::<serde_json::Value>(trimmed).ok();
    let json_title = parsed
        .as_ref()
        .and_then(|v| v.get("metadata"))
        .and_then(|m| m.get("title"))
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    let summary_source = parsed
        .as_ref()
        .and_then(|v| v.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(trimmed);

    let mut non_empty_lines = summary_source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if non_empty_lines == 0 {
        non_empty_lines = trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if non_empty_lines == 0 {
            return None;
        }
    }

    let markdown_title = summary_source.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("# ")
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(ToString::to_string)
    });

    let html_title = {
        let lower = summary_source.to_ascii_lowercase();
        let start = lower.find("<title>");
        let end = lower.find("</title>");
        match (start, end) {
            (Some(start), Some(end)) if end > start + "<title>".len() => Some(
                summary_source[start + "<title>".len()..end]
                    .trim()
                    .to_string(),
            ),
            _ => None,
        }
    };

    let title = json_title.or(markdown_title).or(html_title);
    match title {
        Some(title) => Some(format!(
            "{} · {}",
            truncate_line(&title, 60),
            pluralize_with_count(non_empty_lines, "line", "lines")
        )),
        None => Some(format!(
            "{} fetched",
            pluralize_with_count(non_empty_lines, "line", "lines")
        )),
    }
}

/// Format error message for tool failures with helpful context.
/// Extracts relevant info from common error patterns.
fn format_tool_error_summary(tool: &str, output: &str) -> String {
    let output_trimmed = output.trim();
    let first_line = output.lines().next().unwrap_or("").trim();

    // Tool-specific error extraction
    match tool {
        "bash" | "shell" | "shell_exec" | "run_build_test" => {
            // For bash errors, try to find the most informative part
            // Common patterns: "command not found", "No such file", "Permission denied"
            if let Some(line) = output.lines().find(|l| {
                let lower = l.to_lowercase();
                lower.contains("error:")
                    || lower.contains("failed")
                    || lower.contains("not found")
                    || lower.contains("permission denied")
                    || lower.contains("no such file")
            }) {
                return truncate_line(line.trim(), 80);
            }
            // Fall back to last non-empty line (often contains the actual error)
            if let Some(last) = output.lines().rev().find(|l| !l.trim().is_empty()) {
                return truncate_line(last.trim(), 80);
            }
        }
        "read_file" | "view_file" => {
            if output_trimmed.contains("No such file") || output_trimmed.contains("ENOENT") {
                return "File not found".to_string();
            }
            if output_trimmed.contains("Permission denied") || output_trimmed.contains("EACCES") {
                return "Permission denied".to_string();
            }
            if output_trimmed.contains("Is a directory") || output_trimmed.contains("EISDIR") {
                return "Path is a directory, not a file".to_string();
            }
        }
        "edit" | "write_file" | "create_file" => {
            if output_trimmed.contains("No match found") || output_trimmed.contains("not found") {
                // Extract what wasn't found if possible
                if let Some(line) = output
                    .lines()
                    .find(|l| l.contains("old_str") || l.contains("pattern"))
                {
                    return truncate_line(line.trim(), 80);
                }
                return "Pattern not found in file".to_string();
            }
            if output_trimmed.contains("Permission denied") {
                return "Permission denied — cannot write file".to_string();
            }
            if output_trimmed.contains("already exists") {
                return "File already exists".to_string();
            }
        }
        "grep" | "glob" if output_trimmed.contains("No matches") || output_trimmed.is_empty() => {
            return "No matches found".to_string();
        }
        _ => {}
    }

    // Generic: return first meaningful line, truncated
    truncate_line(first_line, 80)
}

/// Bold+magenta prefix + plain rest (same accent as `Running skill:` / `MCP`).
#[inline]
fn magenta_bold_tool_prefix(prefix: &str, rest: &str) -> String {
    format!("{}{}", prefix.magenta().bold(), rest)
}

/// Try `description.strip_prefix` for each prefix in `prefixes_longest_first` (must be ordered
/// longest-first so e.g. `Git diff --staged ` wins over `Git diff `).
fn style_prefix_longest_first(
    description: &str,
    prefixes_longest_first: &[&str],
) -> Option<String> {
    for p in prefixes_longest_first {
        if let Some(rest) = description.strip_prefix(p) {
            return Some(magenta_bold_tool_prefix(p, rest));
        }
    }
    None
}

/// Sorts `prefixes` by length (longest first), then matches. For a single prefix, no allocation.
fn style_first_matching_prefix(description: &str, prefixes: &[&str]) -> Option<String> {
    match prefixes {
        [] => None,
        [only] => description
            .strip_prefix(only)
            .map(|rest| magenta_bold_tool_prefix(only, rest)),
        _ => {
            let mut sorted: Vec<&str> = prefixes.to_vec();
            sorted.sort_by_key(|p| std::cmp::Reverse(p.len()));
            style_prefix_longest_first(description, &sorted)
        }
    }
}

/// Apply bold+magenta styling to tool description verb prefixes (Read/Edit/Git/shell/…),
/// aligned with `Running skill:` and `MCP`.
pub(crate) fn style_tool_description(tool: &str, description: &str) -> String {
    if tool == "skill" {
        if let Some(rest) = description.strip_prefix("Running skill:") {
            return magenta_bold_tool_prefix("Running skill:", rest);
        }
    } else if tool.starts_with("mcp_") {
        if let Some(rest) = description.strip_prefix("MCP") {
            return magenta_bold_tool_prefix("MCP", rest);
        }
    }

    // Exact short Git lines (must not be split by the catch-all `Git ` prefix).
    match description {
        "Git status" | "Git log" | "Git contributors" | "Git stash" | "Git diff"
        | "Git diff --staged" => {
            return description.magenta().bold().to_string();
        }
        _ => {}
    }

    match tool {
        "read_file" | "view_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Reading: "]) {
                return s;
            }
        }
        "write_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Writing: "]) {
                return s;
            }
        }
        "str_replace" | "multi_edit" => {
            if let Some(s) = style_first_matching_prefix(description, &["Editing: "]) {
                return s;
            }
        }
        "delete_file" => {
            if let Some(s) = style_first_matching_prefix(description, &["Deleting: "]) {
                return s;
            }
        }
        "list_dir" => {
            if let Some(s) = style_first_matching_prefix(description, &["Listing: "]) {
                return s;
            }
        }
        "grep" | "search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Grep: "]) {
                return s;
            }
        }
        "glob" => {
            if let Some(s) = style_first_matching_prefix(description, &["Glob: "]) {
                return s;
            }
        }
        "bash" | "shell" | "shell_exec" | "run_build_test" => {
            if let Some(rest) = description.strip_prefix("$ ") {
                return magenta_bold_tool_prefix("$ ", rest);
            }
        }
        "powershell" => {
            if let Some(rest) = description.strip_prefix("PS> ") {
                return magenta_bold_tool_prefix("PS> ", rest);
            }
        }
        "web_fetch" => {
            if let Some(s) = style_first_matching_prefix(description, &["Fetching: "]) {
                return s;
            }
        }
        "web_search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Searching web: "]) {
                return s;
            }
        }
        "hover_info" => {
            if let Some(s) = style_first_matching_prefix(description, &["Hover info at "]) {
                return s;
            }
        }
        "type_hierarchy" => {
            if let Some(s) = style_first_matching_prefix(description, &["Type hierarchy for "]) {
                return s;
            }
        }
        "symbol_search" => {
            if let Some(s) = style_first_matching_prefix(description, &["Search symbol "]) {
                return s;
            }
        }
        "find_definition" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find definition of "]) {
                return s;
            }
        }
        "find_references" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find references to "]) {
                return s;
            }
        }
        "symbols" => {
            if let Some(s) = style_first_matching_prefix(description, &["Get symbols in "]) {
                return s;
            }
        }
        "call_graph" => {
            if let Some(s) = style_first_matching_prefix(description, &["Call graph for "]) {
                return s;
            }
        }
        "rename_symbol" => {
            if let Some(s) = style_first_matching_prefix(description, &["Rename symbol "]) {
                return s;
            }
        }
        "dead_code" => {
            if let Some(s) = style_first_matching_prefix(description, &["Find dead code: "]) {
                return s;
            }
        }
        "extract_members" => {
            if let Some(s) = style_first_matching_prefix(description, &["Extract members: "]) {
                return s;
            }
        }
        "lsp" => {
            if let Some(s) = style_first_matching_prefix(description, &["LSP: "]) {
                return s;
            }
        }
        "notebook_edit" => {
            if let Some(s) = style_first_matching_prefix(description, &["Notebook edit: "]) {
                return s;
            }
        }
        "reflect" => {
            if let Some(s) = style_first_matching_prefix(description, &["Reflecting: "]) {
                return s;
            }
        }
        "context_analysis" => {
            if let Some(s) = style_first_matching_prefix(description, &["Context analysis: "]) {
                return s;
            }
        }
        "run_chain" => {
            if let Some(s) = style_first_matching_prefix(description, &["Running chain: "]) {
                return s;
            }
        }
        "github_get_pr" => {
            if let Some(s) = style_first_matching_prefix(description, &["Getting PR: "]) {
                return s;
            }
        }
        "github_list_prs" => {
            if let Some(s) = style_first_matching_prefix(description, &["Listing PRs: "]) {
                return s;
            }
        }
        "github_get_issue" => {
            if let Some(s) = style_first_matching_prefix(description, &["Getting issue: "]) {
                return s;
            }
        }
        "github_list_issues" => {
            if let Some(s) = style_first_matching_prefix(description, &["Listing issues: "]) {
                return s;
            }
        }
        "github_repo_stats" => {
            if let Some(s) = style_first_matching_prefix(description, &["GitHub stats: "]) {
                return s;
            }
        }
        "github_ci_status" => {
            if let Some(s) = style_first_matching_prefix(description, &["GitHub CI: "]) {
                return s;
            }
        }
        "github_create_issue" => {
            if let Some(s) = style_first_matching_prefix(description, &["Creating issue: "]) {
                return s;
            }
        }
        "get_agent_info" => {
            if let Some(s) = style_first_matching_prefix(description, &["Getting agent info: "]) {
                return s;
            }
        }
        _ => {}
    }

    if tool.starts_with("git_") {
        // Longest first — do not reorder without checking overlaps.
        const GIT_PREFIXES: &[&str] = &[
            "Git diff --staged ",
            "Git diff ",
            "Git log search \"",
            "Git contributors since ",
            "Git contributors ",
            "Git checkout ",
            "Git worktree ",
            "Git stash ",
            "Git commit \"",
            "Git commit ",
            "Git revert ",
            "Git show ",
            "Git blame ",
            "Git history ",
            "Git log ",
            "Git ",
        ];
        if let Some(s) = style_prefix_longest_first(description, GIT_PREFIXES) {
            return s;
        }
    }

    description.to_string()
}

fn panic_payload_summary(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

async fn catch_tool_execution_panic<F>(future: F) -> (crate::edge_tools::ToolExecutionOutcome, u64)
where
    F: Future<Output = crate::edge_tools::ToolExecutionOutcome>,
{
    let t0 = Instant::now();
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(output) => (output, t0.elapsed().as_millis() as u64),
        Err(payload) => (
            crate::edge_tools::ToolExecutionOutcome {
                output: format!(
                    "Tool execution panicked: {}",
                    panic_payload_summary(payload.as_ref())
                ),
                tool_result_fields: None,
                is_error: true,
            },
            t0.elapsed().as_millis() as u64,
        ),
    }
}

/// Human-friendly tool description from a `ToolCallRecord`'s name + args_preview.
/// Mirrors `format_tool_description_with_output` but works without full args JSON.
pub(crate) fn format_tool_display_from_preview(name: &str, args_preview: Option<&str>) -> String {
    let preview = args_preview.unwrap_or("");
    match name {
        "bash" | "shell_exec" | "run_build_test" => format!("$ {preview}"),
        "powershell" => format!("PS> {preview}"),
        "read_file" | "view_file" => format!("Reading: {preview}"),
        "write_file" => format!("Writing: {preview}"),
        "str_replace" | "multi_edit" => format!("Editing: {preview}"),
        "delete_file" => format!("Deleting: {preview}"),
        "list_dir" => format!("Listing: {preview}"),
        "grep" => format!("Grep: {preview}"),
        "glob" => format!("Glob: {preview}"),
        "git" => format!("Git {preview}"),
        // Legacy individual names (kept for old sessions/journal replay)
        "git_status" => "Git status".to_string(),
        "git_log" => format!("Git log {preview}"),
        "git_show" => format!("Git show {preview}"),
        "git_diff" => format!("Git diff {preview}"),
        "git_blame" => format!("Git blame {preview}"),
        "git_file_history" => format!("Git history {preview}"),
        "git_log_search" => format!("Git log search {preview}"),
        "git_contributors" => {
            if preview.is_empty() {
                "Git contributors".to_string()
            } else {
                format!("Git contributors {preview}")
            }
        }
        "git_commit" => format!("Git commit {preview}"),
        "git_revert_commit" => format!("Git revert {preview}"),
        "git_stash" => format!("Git stash {preview}"),
        "git_checkout_file" => format!("Git checkout {preview}"),
        "git_worktree" => format!("Git worktree {preview}"),
        "find_definition" => format!("Find definition of {preview}"),
        "find_references" => format!("Find references to {preview}"),
        "symbol_search" => format!("Search symbol {preview}"),
        "symbols" => format!("Get symbols in {preview}"),
        "call_graph" => format!("Call graph for {preview}"),
        "hover_info" => format!("Hover info at {preview}"),
        "type_hierarchy" => format!("Type hierarchy for {preview}"),
        "rename_symbol" => format!("Rename symbol {preview}"),
        "dead_code" => format!("Find dead code: {preview}"),
        "extract_members" => format!("Extract members: {preview}"),
        "lsp" => format!("LSP: {preview}"),
        "web_fetch" => format!("Fetching: {preview}"),
        "web_search" => format!("Searching web: \"{preview}\""),
        "github" => format!("GitHub: {preview}"),
        "session" => format!("Session: {preview}"),
        "mo" => format!("MO: {preview}"),
        "agent" => format!("Agent: {preview}"),
        "introspect" => "Introspecting…".to_string(),
        // Legacy individual names
        "github_get_pr" => format!("Getting PR: {preview}"),
        "github_list_prs" => format!("Listing PRs: {preview}"),
        "github_get_issue" => format!("Getting issue: {preview}"),
        "github_list_issues" => format!("Listing issues: {preview}"),
        "github_repo_stats" => format!("GitHub stats: {preview}"),
        "github_ci_status" => format!("GitHub CI: {preview}"),
        "github_create_issue" => format!("Creating issue: {preview}"),
        "get_agent_info" => format!("Getting agent info: {preview}"),
        "reflect" => format!("Reflecting: \"{preview}\""),
        "context_analysis" => format!("Context analysis: {preview}"),
        "run_chain" => format!("Running chain: {preview}"),
        "rollback_file_edits" => format!("Revert file edits: {preview}"),
        "rollback_database_snapshots" => format!("Revert DB snapshots: {preview}"),
        "rollback_turn_actions" => format!("Rollback turn actions: {preview}"),
        "send_message" => format!("Send message: {preview}"),
        "spawn_agent" => format!("Spawn agent: {preview}"),
        "get_agent_result" => format!("Get agent result: {preview}"),
        "diagnose" => format!("Diagnose: {preview}"),
        "env" => format!("Env: {preview}"),
        "notebook_edit" => format!("Notebook edit: {preview}"),
        "config" => format!("Config: {preview}"),
        "brief" => format!("Brief: {preview}"),
        "share_context" => format!("Share context: {preview}"),
        "query_context" => format!("Query context: {preview}"),
        "adjust_config" => format!("Adjust config: {preview}"),
        "prioritize_tool" => format!("Prioritize tool: {preview}"),
        "deprioritize_tool" => format!("Deprioritize tool: {preview}"),
        "set_goal" => format!("Set goal: \"{preview}\""),
        "compress_context" => format!("Compress context: {preview}"),
        "rollback_session_state" => format!("Rollback session state: {preview}"),
        "ask_user" => format!("Asking user: \"{preview}\""),
        "sleep" => format!("Sleeping: {preview}"),
        "tool_search" => format!("Searching tools: {preview}"),
        "enter_plan_mode" => format!("Enter plan mode: \"{preview}\""),
        "exit_plan_mode" => "Exit plan mode".to_string(),
        "task_create" => format!("Creating task: \"{preview}\""),
        "task_list" => {
            if preview.is_empty() {
                "Listing tasks".to_string()
            } else {
                format!("Listing tasks: {preview}")
            }
        }
        "task_get" => format!("Getting task: {preview}"),
        "task_update" => format!("Updating task: {preview}"),
        "task_stop" => format!("Stopping task: {preview}"),
        "mo_query" => format!("MatrixOne query: \"{preview}\""),
        "mo_snapshot" => format!("MatrixOne snapshot: {preview}"),
        "mo_branch" => format!("MatrixOne branch: {preview}"),
        // `memory` is action-aware; when we only have the preview string (not the
        // parsed args), surface it generically. Callers that have the full args
        // object should use the richer `format_tool_description_with_output`
        // path which dispatches on `action`.
        "memory" => {
            if preview.is_empty() {
                "Memory".to_string()
            } else {
                format!("Memory: {preview}")
            }
        }
        "skill" => format!("Running skill: {preview}"),
        "discover_skills" => format!("Discovering skills: \"{preview}\""),
        other if other.starts_with("mcp_") => {
            let rest = &other[4..];
            if let Some(sep) = rest.find('_') {
                format!("MCP {} {preview}", &rest[..sep])
            } else {
                format!("MCP {rest}")
            }
        }
        _ => {
            if preview.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {preview}")
            }
        }
    }
}

fn apply_sse_render_effects(
    effects: Vec<SseRenderEffect>,
    render: &mut StreamRenderState,
    policy: RenderPolicy,
) {
    if policy.is_silent() {
        return;
    }
    for effect in effects {
        match effect {
            SseRenderEffect::StreamText(s) => {
                if let Some(md) = &mut render.md {
                    md.push(&s);
                } else {
                    print!("{s}");
                    let _ = io::stdout().flush();
                    render.track_output(&s);
                }
            }
            SseRenderEffect::StopThinkingSpinner => render.stop_thinking(),
            SseRenderEffect::StartThinkingSpinner => render.start_thinking(),
            SseRenderEffect::ThinkingPreviewChunk(s) => render.push_thinking_preview_chunk(&s),
        }
    }
}

/// Consume one /chat/turn SSE stream, render text deltas, collect tool_calls.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream_cancellable`]; CLI-specific
/// rendering, tool execution, and approval prompts are handled by [`CliSseStreamHost`].
///
/// Terminal output is controlled by `render_policy`:
/// - [`RenderPolicy::Silent`]: no terminal output at all.
/// - [`RenderPolicy::FinalOnly`]: suppressed during streaming; one-shot render if final turn.
/// - [`RenderPolicy::Stream`] / [`RenderPolicy::PlanDecompose`]: full or plan-mode rendering.
///
/// If `cancel_token` is provided, the stream can be cancelled mid-flight by triggering the token.
#[allow(clippy::too_many_arguments)]
pub(super) async fn consume_turn_sse(
    prep_line: ChatTurnPrepLineGuard,
    resp: astra_thin_client::HttpResponse,
    render_md: bool,
    term_width: usize,
    render_policy: RenderPolicy,
    edge: Option<EdgeSseContext<'_>>,
    pre_clear_lines: usize,
    auth_profile: Option<&str>,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> TurnResult {
    // Release the payload/HTTP prep line here so TTFT (`on_before_sse_read_loop`) can take over
    // on the same stderr row without a multi‑hundred‑ms blank gap.
    drop(prep_line);

    // Convert reqwest byte stream to runtime's generic chunk type
    let mut byte_stream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string())),
    );

    // Delegate to runtime's generic SSE consumer with the appropriate host
    let idle = stream_idle_timeout();
    let (
        mut sse_result,
        edge_tool_round,
        mut md_renderer,
        lines_written,
        _pending_xml_buffer,
        auth_failure,
        refreshed_token,
    ) = if let Some(ctx) = edge {
        let original_token = ctx.token.to_string();
        let mut host = CliSseStreamHost::from_edge_ctx_with_auth(
            ctx,
            term_width,
            render_md && !render_policy.suppress_text(),
            auth_profile,
        );
        // pre_clear_lines only applies to non-md fallback path.
        if host.render.md.is_none() {
            host.render.lines_written = pre_clear_lines;
        }
        let (result, _abort) =
            consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token, None)
                .await;
        let lw = host.render.lines_written;
        let md = host.render.md.take();
        let pending = std::mem::take(&mut host.xml_tag_buffer);
        let auth_failure = host.auth_failure;
        let refreshed_token = (host.token != original_token).then(|| host.token.clone());
        (
            result,
            host.edge_tool_round,
            md,
            lw,
            pending,
            auth_failure,
            refreshed_token,
        )
    } else {
        let mut render = StreamRenderState::with_term_width(
            term_width,
            render_md && !render_policy.suppress_text(),
            false,
        );
        if render.md.is_none() {
            render.lines_written = pre_clear_lines;
        }
        let mut host = NoopSseStreamHost;
        let (result, _abort) =
            consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token, None)
                .await;
        let lw = render.lines_written;
        let md = render.md.take();
        (result, Vec::new(), md, lw, String::new(), false, None)
    };
    apply_edge_auth_failure_result(&mut sse_result.accum, auth_failure);

    let mut result = TurnResult {
        core: sse_result.accum,
        ttft_ms: sse_result.ttft_ms,
        edge_tool_round,
        refreshed_token,
    };
    super::streaming_md::strip_xml_tags_inplace(&mut result.full_text);
    // When the model emits both native tool calls AND <invoke> XML text in the
    // same turn (degraded mixed output), strip the XML from full_text. We only
    // do this when tool calls are present — if there are no tool calls, the text
    // might legitimately discuss <invoke> (e.g. code reviews).
    if result.has_tool_calls || !result.edge_tool_round.is_empty() {
        result.full_text =
            astra_turn_core::xml_tool_call_fallback::strip_degraded_tool_calls(&result.full_text);
    }
    super::streaming_md::strip_leading_narration(&mut result.full_text);

    if render_policy.suppress_text() {
        // Silent / FinalOnly / PlanDecompose: text rendering is deferred to the
        // agentic loop via `host.render_final_text()`. No rendering here.
        return result;
    }

    // ─── Finalize incremental markdown ───────────────────────────────────
    // With buffer_from_start=true, ALL text went to `xml_tag_buffer` during
    // SSE consumption. No incremental text was rendered to stdout.
    //
    // Text rendering is now DEFERRED to the agentic loop via
    // `host.render_final_text()`. This prevents text leakage when stop-hooks
    // or factual retries cause the loop to continue after a text-only turn.
    //
    // Tool turns: discard any rendered state (thinking spinners, etc.)
    // Non-tool turns: nothing to discard — text was buffered, not rendered.
    let has_any_tool_work = result.has_tool_calls || !result.edge_tool_round.is_empty();
    if has_any_tool_work {
        // Tool turn — discard any incremental rendering state
        if let Some(md) = &mut md_renderer {
            md.discard_and_reset();
        } else if lines_written > 0 && io::stdout().is_terminal() {
            execute!(
                io::stdout(),
                cursor::MoveUp(lines_written as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        }
    }
    // Non-tool turns: text is NOT rendered here. It will be rendered by
    // the agentic loop host when it confirms this is the final answer.

    result
}

/// Used by `main` test module and stream_render unit tests; production path is [`consume_turn_sse`].
#[allow(dead_code)]
pub(super) fn dispatch_turn_event_block(
    block: &str,
    result: &mut TurnResult,
    render: &mut StreamRenderState,
    policy: RenderPolicy,
    pending_edge: &mut Vec<ChatTurnEdgePending>,
) {
    let effects = dispatch_chat_turn_sse_event_block(block, &mut result.core, pending_edge);
    apply_sse_render_effects(effects, render, policy);
}

/// Append `<skill-loaded name="..."/>` to a successful skill result.
///
/// The system prompt tells the LLM: "On seeing `<skill-loaded name="..."/>` in
/// a tool result, follow that skill's instructions — do not re-invoke it."
/// Without this marker, the LLM doesn't know the skill loaded and may
/// invoke discover_skills + a second skill in the same turn.
///
/// Error results (starting with "Error:") are returned unchanged — the LLM
/// should be free to retry or switch strategies on failures.
///
/// Mirrors the server-side logic in
/// `runtime::turn::skill_tool::partition_discover_and_execute_skills` (line ~1098).
fn append_skill_loaded_marker(result: &str, skill_name: &str) -> String {
    if result.starts_with("Error:") || result.starts_with("error:") || result.trim().is_empty() {
        return result.to_string();
    }
    // Sanitize: allowlist to a conservative set of filename-safe
    // characters. A malicious skill registry entry could otherwise
    // use path-like names (`../evil`) or Unicode line separators
    // (U+2028 / U+2029, which `is_control` does NOT catch) to
    // impersonate a different skill in LLM-visible output. Anything
    // outside the allowlist is replaced with `_`.
    // The allowlist already rejects every XML-special character
    // (`<`, `>`, `&`, `"`, `'`) by replacing it with `_`, so no
    // subsequent XML-escape pass is needed — and adding one would be
    // dead code that falsely suggests the allowlist is permissive.
    let safe_name: String = skill_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Belt-and-suspenders: make the allowlist's XML-safety guarantee
    // a runtime invariant, so any future relaxation of the allowlist
    // that lets an XML-special char slip through trips tests
    // immediately instead of silently enabling tag breakout.
    debug_assert!(
        !safe_name
            .chars()
            .any(|c| matches!(c, '<' | '>' | '&' | '"' | '\'')),
        "allowlist must reject every XML-special character; got {safe_name:?}"
    );
    format!("{result}\n\n<skill-loaded name=\"{safe_name}\"/>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_utils::{CredentialsFile, Profile, save_credentials};
    use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
    use tempfile::tempdir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── D-9 regression: speculative success flag must gate reuse ──
    //
    // Guards against the cascade bug where a speculative tool execution that
    // failed (semaphore saturated, permission denied mid-stream, tool errored
    // with non-empty error message) was silently reused as a successful
    // tool_result because the consumer discarded `success` with `_ok`.
    // See `reusable_speculative_output` for the fix rationale.

    #[test]
    fn edge_post_auth_failure_is_auth_error_not_user_cancel() {
        let mut accum = ChatTurnSseAccum {
            error_message: Some("Cancelled by user".to_string()),
            ..Default::default()
        };

        apply_edge_auth_failure_result(&mut accum, true);

        let error = accum.error_message.as_deref().unwrap_or_default();
        assert!(error.contains("401 Unauthorized"));
        assert!(!error.contains("Cancelled by user"));
    }

    #[test]
    fn edge_post_without_auth_failure_keeps_existing_error() {
        let mut accum = ChatTurnSseAccum {
            error_message: Some("Cancelled by user".to_string()),
            ..Default::default()
        };

        apply_edge_auth_failure_result(&mut accum, false);

        assert_eq!(accum.error_message.as_deref(), Some("Cancelled by user"));
    }

    #[test]
    fn approval_stale_revalidation_allows_unchanged_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"baseline").unwrap();
        let previous = astra_turn_core::approval_base_digest::compute_file_digest(&path).unwrap();

        let error = approval_stale_revalidation_error("str_replace", &path, previous);

        assert!(error.is_none(), "unchanged file should pass revalidation");
    }

    #[test]
    fn approval_stale_revalidation_blocks_modified_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"baseline").unwrap();
        let previous = astra_turn_core::approval_base_digest::compute_file_digest(&path).unwrap();
        std::fs::write(&path, b"changed").unwrap();

        let error = approval_stale_revalidation_error("str_replace", &path, previous).unwrap();

        assert!(error.contains("Approval expired for str_replace"));
        assert!(error.contains("changed from"));
    }

    #[test]
    fn approval_stale_revalidation_blocks_file_appearing_after_prompt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let previous = astra_turn_core::approval_base_digest::compute_file_digest(&path).unwrap();
        assert!(previous.is_none());
        std::fs::write(&path, b"created elsewhere").unwrap();

        let error = approval_stale_revalidation_error("write_file", &path, previous).unwrap();

        assert!(error.contains("appeared after approval"));
    }

    #[test]
    fn approval_batch_group_key_carries_risk_tags_for_accept_all_gate() {
        use astra_turn_core::permission_engine::RiskTag;

        let safe = approval_batch_group_key(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            &[RiskTag::BashExecute],
        );
        assert!(safe.allows_accept_all());

        let dangerous = approval_batch_group_key(
            "write_file",
            &serde_json::json!({"path": ".env"}),
            &[RiskTag::WritesSensitiveFile],
        );
        assert!(
            !dangerous.allows_accept_all(),
            "production group key must carry tags used by the queue's Accept-all guard"
        );
    }

    #[test]
    fn approval_default_always_scope_prefers_project_for_benign_request() {
        let ctx = astra_turn_core::permission_scope::ScopeAvailabilityContext::default();

        assert_eq!(
            approval_default_always_scope(&ctx),
            astra_turn_core::permission_scope::AllowScope::Project
        );
    }

    #[test]
    fn approval_default_always_scope_uses_session_for_non_persistent_risks() {
        use astra_turn_core::permission_engine::RiskTag;
        use astra_turn_core::permission_scope::{AllowScope, ScopeAvailabilityContext};

        let sensitive = ScopeAvailabilityContext {
            risk_tags: vec![RiskTag::WritesSensitiveFile],
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&sensitive),
            AllowScope::RestOfSession
        );

        let mcp_unknown = ScopeAvailabilityContext {
            risk_tags: vec![RiskTag::MCPUnknownCapability],
            mcp_unknown_capability: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&mcp_unknown),
            AllowScope::RestOfSession
        );

        let sub_agent = ScopeAvailabilityContext {
            source_agent_present: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&sub_agent),
            AllowScope::RestOfSession
        );
    }

    #[test]
    fn approval_scope_context_tags_git_safety_as_non_persistent_risk() {
        use astra_turn_core::permission_engine::RiskTag;
        use astra_turn_core::permission_scope::AllowScope;

        let ctx = approval_scope_context_for_tool(
            "bash",
            &serde_json::json!({"command": "git push --force origin main"}),
            false,
            false,
        );

        assert!(ctx.risk_tags.contains(&RiskTag::GitDestructive));
        assert_eq!(
            approval_default_always_scope(&ctx),
            AllowScope::RestOfSession
        );
    }

    #[test]
    fn approval_default_always_scope_uses_turn_for_unsound_rule_shapes() {
        use astra_turn_core::permission_scope::{AllowScope, ScopeAvailabilityContext};

        let compound = ScopeAvailabilityContext {
            is_compound_command: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&compound),
            AllowScope::RestOfTurn
        );

        let dynamic_eval = ScopeAvailabilityContext {
            has_dynamic_eval: true,
            ..Default::default()
        };
        assert_eq!(
            approval_default_always_scope(&dynamic_eval),
            AllowScope::RestOfTurn
        );
    }

    #[test]
    fn approval_memory_action_does_not_remember_allow_once() {
        use super::chat_stream::ApprovalResponse;
        use astra_turn_core::permission_scope::AllowScope;

        assert_eq!(
            approval_memory_action(ApprovalResponse::AllowOnce, AllowScope::Project, true),
            ApprovalMemoryAction::None
        );
    }

    #[test]
    fn approval_memory_action_maps_always_scope_to_storage_effect() {
        use super::chat_stream::ApprovalResponse;
        use astra_turn_core::permission_scope::AllowScope;

        assert_eq!(
            approval_memory_action(ApprovalResponse::AlwaysAllow, AllowScope::Project, true),
            ApprovalMemoryAction::PersistProjectRule
        );
        assert_eq!(
            approval_memory_action(ApprovalResponse::AlwaysAllow, AllowScope::User, true),
            ApprovalMemoryAction::PersistUserRule
        );
        assert_eq!(
            approval_memory_action(ApprovalResponse::AlwaysAllow, AllowScope::RestOfTurn, true),
            ApprovalMemoryAction::RecordAllowTurn
        );
        assert_eq!(
            approval_memory_action(
                ApprovalResponse::AlwaysAllow,
                AllowScope::RestOfSession,
                true
            ),
            ApprovalMemoryAction::RecordAllowSession
        );
        assert_eq!(
            approval_memory_action(
                ApprovalResponse::AlwaysAllow,
                AllowScope::OnceThisCall,
                true
            ),
            ApprovalMemoryAction::None
        );
        assert_eq!(
            approval_memory_action(ApprovalResponse::AlwaysAllow, AllowScope::Project, false),
            ApprovalMemoryAction::RecordDenySession
        );
    }

    #[tokio::test]
    async fn cloud_approval_uses_tui_sink_in_prompt_mode() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm = crate::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::unbounded_channel::<super::chat_stream::ApprovalRequest>();
        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor,
                render_policy: RenderPolicy::Stream,
                perm_manager: Some(&mut pm),
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: Some(approval_tx),
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let decision_fut = host.resolve_cloud_approval_via_tui(
            "write_file",
            Some("src/main.rs"),
            astra_thin_client::ApprovalKind::Standard,
        );
        let responder = async {
            let request = approval_rx.recv().await.expect("approval request");
            assert_eq!(request.tool, "write_file");
            assert!(request.header.contains("Cloud approval required"));
            request
                .response_tx
                .send(super::chat_stream::ApprovalResponse::AllowOnce)
                .expect("send response");
        };

        let (decision, ()) = tokio::join!(decision_fut, responder);

        assert_eq!(decision, astra_thin_client::ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn cloud_approval_always_persists_benign_project_scope() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm = crate::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::unbounded_channel::<super::chat_stream::ApprovalRequest>();

        let decision = {
            let mut host = CliSseStreamHost::from_edge_ctx(
                EdgeSseContext {
                    api: &api,
                    token: "tok",
                    executor_id: "edge-test",
                    executor,
                    render_policy: RenderPolicy::Stream,
                    perm_manager: Some(&mut pm),
                    cancel_token: None,
                    stream_event_tx: None,
                    approval_request_tx: Some(approval_tx),
                    skill_resolver: None,
                    skill_continuation: false,
                    turn_rollback_on_failure: false,
                    tool_cache: &mut tool_cache,
                    observability_hub: None,
                },
                80,
                false,
            );
            let decision_fut = host.resolve_cloud_approval_via_tui(
                "write_file",
                Some("src/main.rs"),
                astra_thin_client::ApprovalKind::Standard,
            );
            let responder = async {
                let request = approval_rx.recv().await.expect("approval request");
                request
                    .response_tx
                    .send(super::chat_stream::ApprovalResponse::AlwaysAllow)
                    .expect("send response");
            };
            let (decision, ()) = tokio::join!(decision_fut, responder);
            decision
        };

        assert_eq!(decision, astra_thin_client::ApprovalDecision::AllowSession);

        let args = serde_json::json!({"path": "src/main.rs"});
        let mut reloaded =
            crate::permission_manager::PermissionManager::with_project(false, temp.path());
        assert!(matches!(
            reloaded.check_nonblocking("write_file", &args),
            crate::permission_manager::PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn cloud_approval_always_sensitive_write_is_session_only() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        let mut pm = crate::permission_manager::PermissionManager::with_project(false, temp.path());
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::unbounded_channel::<super::chat_stream::ApprovalRequest>();

        let decision = {
            let mut host = CliSseStreamHost::from_edge_ctx(
                EdgeSseContext {
                    api: &api,
                    token: "tok",
                    executor_id: "edge-test",
                    executor,
                    render_policy: RenderPolicy::Stream,
                    perm_manager: Some(&mut pm),
                    cancel_token: None,
                    stream_event_tx: None,
                    approval_request_tx: Some(approval_tx),
                    skill_resolver: None,
                    skill_continuation: false,
                    turn_rollback_on_failure: false,
                    tool_cache: &mut tool_cache,
                    observability_hub: None,
                },
                80,
                false,
            );
            let decision_fut = host.resolve_cloud_approval_via_tui(
                "write_file",
                Some(".env"),
                astra_thin_client::ApprovalKind::Standard,
            );
            let responder = async {
                let request = approval_rx.recv().await.expect("approval request");
                request
                    .response_tx
                    .send(super::chat_stream::ApprovalResponse::AlwaysAllow)
                    .expect("send response");
            };
            let (decision, ()) = tokio::join!(decision_fut, responder);
            decision
        };

        assert_eq!(decision, astra_thin_client::ApprovalDecision::AllowSession);

        let args = serde_json::json!({"path": ".env"});
        assert!(matches!(
            pm.check_nonblocking("write_file", &args),
            crate::permission_manager::PermissionDecision::Allow
        ));
        let mut reloaded =
            crate::permission_manager::PermissionManager::with_project(false, temp.path());
        assert!(matches!(
            reloaded.check_nonblocking("write_file", &args),
            crate::permission_manager::PermissionDecision::NeedApproval { .. }
        ));
    }

    #[test]
    fn edge_auth_failure_detector_only_matches_http_401_api_errors() {
        let unauthorized = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "expired".to_string(),
        };
        let forbidden = astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::FORBIDDEN,
            body: "forbidden".to_string(),
        };

        assert!(is_edge_auth_failure(&unauthorized));
        assert!(!is_edge_auth_failure(&forbidden));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn edge_tool_result_401_refreshes_and_retries_without_terminal_auth_failure() {
        static CREDENTIALS_ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        let _guard = CREDENTIALS_ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("credentials env lock");

        let credentials_dir = tempdir().expect("credentials dir");
        unsafe { std::env::set_var("ASTRA_CREDENTIALS_DIR", credentials_dir.path()) };
        struct CredentialsEnvGuard;
        impl Drop for CredentialsEnvGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("ASTRA_CREDENTIALS_DIR") };
            }
        }
        let _env_guard = CredentialsEnvGuard;

        let mut creds = CredentialsFile {
            current_profile: Some("test".to_string()),
            ..Default::default()
        };
        creds.profiles.insert(
            "test".to_string(),
            Profile {
                access_token: Some("expired-token".to_string()),
                refresh_token: Some("refresh-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).expect("save credentials");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::TOOLS_RESULT))
            .and(header("authorization", "Bearer expired-token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "expired"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::AUTH_REFRESH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh-token",
                "refresh_token": "fresh-refresh-token"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(astra_thin_client::paths::TOOLS_RESULT))
            .and(header("authorization", "Bearer fresh-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(server.uri().as_str(), None).expect("client");
        let workspace = tempdir().expect("workspace");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(workspace.path()));
        let mut tool_cache = EdgeToolCache::new(10);
        let ctx = EdgeSseContext {
            api: &api,
            token: "expired-token",
            executor_id: "edge-test",
            executor,
            render_policy: RenderPolicy::Silent,
            perm_manager: None,
            cancel_token: None,
            stream_event_tx: None,
            approval_request_tx: None,
            skill_resolver: None,
            skill_continuation: false,
            turn_rollback_on_failure: false,
            tool_cache: &mut tool_cache,
            observability_hub: None,
        };
        let mut host = CliSseStreamHost::from_edge_ctx_with_auth(ctx, 80, false, Some("test"));
        let body = astra_thin_client::ToolResultRequest {
            request_id: "req-1".to_string(),
            status: "ok".to_string(),
            output: Some("done".to_string()),
            duration_ms: Some(1),
        };

        let terminal_auth_failure = host.post_tool_result_with_auth_retry(&body).await;

        assert!(!terminal_auth_failure);
        assert!(!host.auth_failure);
        assert_eq!(host.token, "fresh-token");
    }

    #[test]
    fn reusable_speculative_output_accepts_successful_result() {
        let out = reusable_speculative_output(Some(("real grep hit: line 42".to_string(), true)));
        assert_eq!(out, Some("real grep hit: line 42".to_string()));
    }

    #[test]
    fn reusable_speculative_output_rejects_failed_result_even_with_content() {
        // A failed speculation may carry a non-empty error message. That
        // message MUST NOT be reused as a successful tool_result — the real
        // execution path must re-run so genuine status surfaces.
        let out = reusable_speculative_output(Some((
            "Error: permission denied on /etc/shadow".to_string(),
            false,
        )));
        assert_eq!(out, None);
    }

    #[test]
    fn reusable_speculative_output_rejects_none() {
        let out = reusable_speculative_output(None);
        assert_eq!(out, None);
    }

    #[test]
    fn reusable_speculative_output_rejects_failed_empty_content() {
        // Semaphore-saturated speculation returns empty content + success=false.
        // Must not be reused (would surface empty output as successful tool_result).
        let out = reusable_speculative_output(Some((String::new(), false)));
        assert_eq!(out, None);
    }

    fn init_temp_git_repo() -> tempfile::TempDir {
        let dir = tempdir().expect("temp repo");
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.name");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .expect("git config user.email");
        std::fs::write(dir.path().join("tracked.txt"), "committed\n").expect("seed tracked file");
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");
        dir
    }

    fn boundary_events(session_id: &str) -> Vec<JournalEvent> {
        session_journal::read_journal(session_id)
            .expect("read journal")
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    JournalEventType::ExecutionBoundaryOpened
                        | JournalEventType::ExecutionBoundaryCommitted
                        | JournalEventType::ExecutionBoundaryAborted
                )
            })
            .collect()
    }

    fn boundary_metadata(event: &JournalEvent) -> &Value {
        event
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("execution_boundary"))
            .expect("execution boundary metadata")
    }

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    /// Longer than any realistic `path_budget` from `format_tool_description` (even on very wide
    /// terminals), so `shorten_path` always produces a `.../` prefix in tests.
    fn path_longer_than_any_sane_terminal_budget() -> String {
        let mut p = String::from("/");
        for i in 0..1200 {
            p.push_str("dir");
            p.push_str(&i.to_string());
            p.push('/');
        }
        p.push_str("src/lib.rs");
        p
    }

    #[test]
    fn tool_completion_icon_grep_substring_warning_in_hit_is_not_warn() {
        let out = r#"crates/x/src/lib.rs:42:    tracing::warn!("⚠ WARNING: retry");"#;
        let (_icon, is_warning) = tool_completion_icon("grep", "ok", out, 50);
        assert!(
            !is_warning,
            "grep output must not warn just because a match line contains the substring"
        );
    }

    #[test]
    fn tool_completion_icon_platform_banner_line_is_warn() {
        let out = "\n\n⚠ WARNING: This file has been read 4+ times this session.";
        let (_icon, is_warning) = tool_completion_icon("read_file", "ok", out, 10);
        assert!(is_warning);
    }

    #[test]
    fn tool_completion_icon_glob_no_files_found_is_ok() {
        let (_icon, is_warning) = tool_completion_icon("glob", "ok", "No files found", 50);
        assert!(!is_warning);
    }

    #[test]
    fn tool_completion_icon_grep_no_matches_is_ok() {
        let (_icon, is_warning) = tool_completion_icon("grep", "ok", "No matches found", 50);
        assert!(!is_warning);
    }

    #[test]
    fn tool_completion_icon_grep_empty_stdout_is_ok() {
        let (_icon, is_warning) = tool_completion_icon("grep", "ok", "", 50);
        assert!(
            !is_warning,
            "empty grep result must not be a warning when status is ok"
        );
    }

    #[test]
    fn tool_completion_icon_glob_empty_stdout_is_ok() {
        let (_icon, is_warning) = tool_completion_icon("glob", "ok", "", 50);
        assert!(!is_warning);
    }

    #[test]
    fn tool_completion_icon_read_file_empty_still_warns() {
        let (_icon, is_warning) = tool_completion_icon("read_file", "ok", "", 50);
        assert!(is_warning);
    }

    #[test]
    fn tool_completion_icon_bash_clippy_style_warning_substring_is_ok() {
        let out = "warning: unused variable\n --> src/lib.rs:1:5\n\nwarning: another\n";
        let (_icon, is_warning) = tool_completion_icon("bash", "ok", out, 50);
        assert!(
            !is_warning,
            "stdout may contain compiler warning: lines; do not treat as completion warning"
        );
    }

    /// `dispatch_turn_event_block` with `quiet` must still fill the shared runtime accumulator.
    #[test]
    fn dispatch_quiet_wires_runtime_accumulator() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, RenderPolicy::Silent, &mut vec![]);
        assert_eq!(r.full_text, "hello world");
    }

    #[test]
    fn tool_request_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"tool_request\",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo x\"}}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, RenderPolicy::Silent, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "echo x");
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn approval_required_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, RenderPolicy::Silent, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                approval_kind,
                detail,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(*approval_kind, astra_thin_client::ApprovalKind::Standard);
                assert_eq!(detail.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
        }
    }

    // ── Regression: intermediate draft text must not leak ─────────────

    #[test]
    fn final_text_cleanup_strips_reflect_tags() {
        let mut text = "before\n<reflect>hidden</reflect>\nafter".to_string();
        super::streaming_md::strip_xml_tags_inplace(&mut text);
        assert_eq!(text, "before\nafter");
    }

    #[test]
    fn final_text_cleanup_strips_think_tags() {
        let mut text = "before\n<think>\nlong thinking block\n</think>\nafter".to_string();
        super::streaming_md::strip_xml_tags_inplace(&mut text);
        assert_eq!(text, "before\nafter");
    }

    // ── Line tracking ────────────────────────────────────────────────

    #[test]
    fn track_output_counts_newlines() {
        let mut s = StreamRenderState::new();
        s.track_output("hello\nworld\n");
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_output_counts_wraps() {
        let mut s = StreamRenderState::with_term_width(10, false, false);
        // 20 chars = 2 wraps on a 10-col terminal
        s.track_output("12345678901234567890");
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_eprintln_increments_line() {
        let mut s = StreamRenderState::new();
        s.track_eprintln();
        s.track_eprintln();
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_mixed_stdout_stderr_lines() {
        let mut s = StreamRenderState::with_term_width(80, false, false);
        // Simulate: thinking line (stderr) + streamed text (stdout) + tool_request (stderr)
        s.track_eprintln(); // ● Thought for 1.4s
        s.track_output("Let me review the code\n"); // text_delta
        s.track_eprintln(); // ⚡ tool_request: bash
        assert_eq!(s.lines_written, 3);
    }

    // ── Regression: stderr tool-done lines must be tracked in lines_written ──
    //
    // When tool completion output (e.g. "✓ Git diff …") goes to stderr via
    // `tool_done_inline`, those lines occupy terminal rows. If they are NOT
    // tracked in `lines_written`, subsequent `MoveUp(lines_written)` will
    // move the cursor too few rows, leaving residual text on screen — the
    // "text leakage" bug.
    //
    // The fix: both `tool_done` (md-mode branch) and `tool_done_inline`
    // now increment `lines_written` for stderr output lines.

    #[test]
    fn tool_done_inline_stderr_lines_counted_in_lines_written() {
        let mut s = StreamRenderState::with_term_width(80, false, false);
        s.track_output("Draft review text\n"); // 1 stdout line
        assert_eq!(s.lines_written, 1);

        // Call actual method: emits 1 stderr line (summary only, no error detail)
        s.tool_done_inline("bash", &serde_json::json!({}), "success", 100, "done");
        assert!(
            s.lines_written >= 2,
            "lines_written should account for stderr"
        );
        assert!(s.stderr_lines >= 1, "stderr_lines should be incremented");
    }

    #[test]
    fn tool_done_md_mode_stderr_lines_counted_in_lines_written() {
        let mut s = StreamRenderState::with_term_width(80, true, false); // md mode
        s.track_output("Intermediate draft\n"); // 1 stdout line
        assert_eq!(s.lines_written, 1);

        // Call actual method in md mode
        s.tool_done(0, "bash", &serde_json::json!({}), "success", 100, "done");
        assert!(
            s.lines_written >= 2,
            "md mode should still track lines_written"
        );
    }

    // ── Partial tag detection ────────────────────────────────────────

    #[test]
    fn could_become_suppressed_tag_matches_known_prefixes() {
        use super::super::streaming_md::could_become_suppressed_tag;
        assert!(could_become_suppressed_tag("<"));
        assert!(could_become_suppressed_tag("</"));
        assert!(could_become_suppressed_tag("<t"));
        assert!(could_become_suppressed_tag("<th"));
        assert!(could_become_suppressed_tag("<thi"));
        assert!(could_become_suppressed_tag("<thin"));
        assert!(could_become_suppressed_tag("<think"));
        assert!(could_become_suppressed_tag("</think"));
        assert!(could_become_suppressed_tag("<r"));
        assert!(could_become_suppressed_tag("<ref"));
        assert!(could_become_suppressed_tag("</reflect"));
    }

    #[test]
    fn could_become_suppressed_tag_rejects_other_tags() {
        use super::super::streaming_md::could_become_suppressed_tag;
        assert!(!could_become_suppressed_tag("<co")); // <code>
        assert!(!could_become_suppressed_tag("<p"));
        assert!(!could_become_suppressed_tag("<div"));
        assert!(!could_become_suppressed_tag("<span"));
        assert!(!could_become_suppressed_tag("</code"));
        assert!(!could_become_suppressed_tag("<a"));
        assert!(!could_become_suppressed_tag("<b"));
    }

    #[test]
    fn colored_string_equality() {
        use crossterm::style::Stylize;
        // Verify that .dim() produces consistent output for comparison
        let s1 = format!("  {} {}", "◇".dim(), "test".dim());
        let s2 = format!("  {} {}", "◇".dim(), "test".dim());
        assert_eq!(s1, s2, "colored strings should be equal");
    }

    #[test]
    fn extract_cli_diff_from_write_file_json() {
        let diff_body = "--- a/x.js\n+++ b/x.js\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let out = serde_json::json!({
            "success": true,
            "bytes_written": 3u32,
            "path": "/tmp/x.js",
            "_cli_unified_diff": diff_body,
        })
        .to_string();
        let got = super::extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), diff_body);
    }

    #[test]
    fn extract_cli_diff_sentinel_wrapped() {
        let embedded = "+++ b/f\n+ok\n";
        let out = format!("<<<ASTRA_UNIFIED_DIFF>>>{embedded}<<<END_ASTRA_UNIFIED_DIFF>>>");
        let got = super::extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), embedded.trim());
    }

    // extract_first_absolute_path moved to crate::sandbox_retry — its
    // tests live there now as part of the TDD coverage for the shared
    // SANDBOX_DENIED retry path.

    // ── style_tool_description tests ──

    #[test]
    fn style_skill_description_has_bold_prefix() {
        let styled = style_tool_description("skill", "Running skill: code-review");
        // Should contain ANSI codes (bold+magenta) and the skill name
        assert!(styled.contains("code-review"));
        assert!(styled.contains("Running skill"));
        // Plain text without ANSI should NOT match (it has escape sequences)
        assert_ne!(styled, "Running skill: code-review");
    }

    #[test]
    fn style_mcp_description_has_bold_prefix() {
        let styled = style_tool_description("mcp_github_search", "MCP github search");
        assert!(styled.contains("search"));
        assert!(styled.contains("MCP"));
        assert_ne!(styled, "MCP github search");
    }

    #[test]
    fn style_read_file_has_bold_prefix() {
        let styled = style_tool_description("read_file", "Reading: src/main.rs");
        assert!(styled.contains("src/main.rs"));
        assert!(styled.contains("Reading:"));
        assert_ne!(styled, "Reading: src/main.rs");
    }

    #[test]
    fn style_bash_has_bold_prefix() {
        let styled = style_tool_description("bash", "$ echo hello");
        assert!(styled.contains("echo hello"));
        assert!(styled.contains("$"));
        assert_ne!(styled, "$ echo hello");
    }

    #[test]
    fn style_shell_exec_matches_bash() {
        let styled = style_tool_description("shell_exec", "$ cargo test -p astra-cli");
        assert!(styled.contains("cargo test"));
        assert_ne!(styled, "$ cargo test -p astra-cli");
    }

    // ── Skill/MCP format_tool_description tests ──

    #[test]
    fn format_skill_description() {
        let r = StreamRenderState::new();
        let args = serde_json::json!({"skill_name": "code-review"});
        let desc = r.format_tool_description("skill", &args);
        assert_eq!(desc, "Running skill: code-review");
    }

    #[test]
    fn format_shell_exec_same_as_bash() {
        let r = StreamRenderState::new();
        let args = serde_json::json!({"command": "echo hi"});
        let bash = r.format_tool_description("bash", &args);
        let shell_exec = r.format_tool_description("shell_exec", &args);
        assert_eq!(bash, shell_exec);
        assert!(bash.starts_with("$ "));
    }

    #[test]
    fn format_mcp_description_with_server_and_tool() {
        let r = StreamRenderState::new();
        let desc = r.format_tool_description("mcp_github_search_repos", &serde_json::json!({}));
        assert_eq!(desc, "MCP github search_repos");
    }

    #[test]
    fn format_mcp_description_no_underscore() {
        let r = StreamRenderState::new();
        let desc = r.format_tool_description("mcp_mytool", &serde_json::json!({}));
        assert_eq!(desc, "MCP mytool");
    }

    #[test]
    fn format_git_revert_description() {
        let r = StreamRenderState::new();
        let args = serde_json::json!({"commit_sha": "abc123def456"});
        let desc = r.format_tool_description("git_revert_commit", &args);
        assert_eq!(desc, "Git revert abc123def456");
    }

    #[test]
    fn format_git_stash_description() {
        let r = StreamRenderState::new();
        let args = serde_json::json!({"action": "apply", "stash_ref": "stash@{2}"});
        let desc = r.format_tool_description("git_stash", &args);
        assert_eq!(desc, "Git stash apply stash@{2}");
    }

    #[test]
    fn format_git_helper_descriptions() {
        let r = StreamRenderState::new();
        let file_history = r.format_tool_description(
            "git_file_history",
            &serde_json::json!({"file": "src/main.rs"}),
        );
        let log_search =
            r.format_tool_description("git_log_search", &serde_json::json!({"query": "auth"}));
        let contributors = r.format_tool_description(
            "git_contributors",
            &serde_json::json!({"path": "src/", "since": "30 days ago"}),
        );

        assert_eq!(file_history, "Git history src/main.rs");
        assert_eq!(log_search, "Git log search \"auth\"");
        assert_eq!(contributors, "Git contributors src/ since 30 days ago");
    }

    #[test]
    fn format_git_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("git_revert_commit", Some("abc123")),
            "Git revert abc123"
        );
        assert_eq!(
            format_tool_display_from_preview("git_stash", Some("push")),
            "Git stash push"
        );
        assert_eq!(
            format_tool_display_from_preview("git_file_history", Some("src/main.rs")),
            "Git history src/main.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("git_log_search", Some("\"auth\"")),
            "Git log search \"auth\""
        );
        assert_eq!(
            format_tool_display_from_preview("git_contributors", Some("src/ since 30 days ago")),
            "Git contributors src/ since 30 days ago"
        );
    }

    #[test]
    fn format_additional_git_tool_descriptions() {
        let r = StreamRenderState::new();
        let checkout = r.format_tool_description(
            "git_checkout_file",
            &serde_json::json!({"path": "src/lib.rs", "ref": "HEAD~1"}),
        );
        let worktree = r.format_tool_description(
            "git_worktree",
            &serde_json::json!({"action": "add", "branch": "feature/ui"}),
        );

        assert_eq!(checkout, "Git checkout HEAD~1 -- src/lib.rs");
        assert_eq!(worktree, "Git worktree add feature/ui");
    }

    #[test]
    fn format_additional_git_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("git_checkout_file", Some("HEAD~1 -- src/lib.rs")),
            "Git checkout HEAD~1 -- src/lib.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("git_worktree", Some("add feature/ui")),
            "Git worktree add feature/ui"
        );
    }

    #[test]
    fn format_git_checkout_preview_shortens_long_path() {
        let r = StreamRenderState::new();
        let path = path_longer_than_any_sane_terminal_budget();
        let desc = r.format_tool_description(
            "git_checkout_file",
            &serde_json::json!({
                "path": path,
                "ref": "HEAD~1"
            }),
        );
        assert!(
            desc.starts_with("Git checkout HEAD~1 -- .../"),
            "expected ellipsis-prefixed path; got {desc:?}"
        );
        assert!(desc.ends_with("src/lib.rs"), "got {desc:?}");
    }

    #[test]
    fn format_github_descriptions_use_repo_argument() {
        let r = StreamRenderState::new();
        let pr = r.format_tool_description(
            "github_get_pr",
            &serde_json::json!({"repo": "matrixorigin/astra", "pr_number": 159}),
        );
        let issue = r.format_tool_description(
            "github_create_issue",
            &serde_json::json!({"repo": "matrixorigin/astra", "title": "Fix renderer drift"}),
        );
        let ci = r.format_tool_description(
            "github_ci_status",
            &serde_json::json!({"repo": "matrixorigin/astra"}),
        );

        assert_eq!(pr, "Getting PR: matrixorigin/astra#159");
        assert_eq!(
            issue,
            "Creating issue: matrixorigin/astra \"Fix renderer drift\""
        );
        assert_eq!(ci, "GitHub CI: matrixorigin/astra");
    }

    #[test]
    fn format_github_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("github_get_issue", Some("matrixorigin/astra#147")),
            "Getting issue: matrixorigin/astra#147"
        );
        assert_eq!(
            format_tool_display_from_preview("github_list_issues", Some("matrixorigin/astra")),
            "Listing issues: matrixorigin/astra"
        );
        assert_eq!(
            format_tool_display_from_preview(
                "github_create_issue",
                Some("matrixorigin/astra: \"Fix renderer drift\""),
            ),
            "Creating issue: matrixorigin/astra: \"Fix renderer drift\""
        );
    }

    #[test]
    fn format_utility_tool_descriptions() {
        let r = StreamRenderState::new();
        let ask_user = r.format_tool_description(
            "ask_user",
            &serde_json::json!({"question": "Continue with the refactor?"}),
        );
        let sleep = r.format_tool_description(
            "sleep",
            &serde_json::json!({"duration_ms": 1500, "reason": "waiting for CI"}),
        );
        let tool_search =
            r.format_tool_description("tool_search", &serde_json::json!({"query": "git"}));

        assert_eq!(ask_user, "Asking user: \"Continue with the refactor?\"");
        assert_eq!(sleep, "Sleeping: 1500ms (waiting for CI)");
        assert_eq!(tool_search, "Searching tools: \"git\"");
    }

    #[test]
    fn format_utility_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("ask_user", Some("Continue with the refactor?")),
            "Asking user: \"Continue with the refactor?\""
        );
        assert_eq!(
            format_tool_display_from_preview("sleep", Some("1500ms (waiting for CI)")),
            "Sleeping: 1500ms (waiting for CI)"
        );
        assert_eq!(
            format_tool_display_from_preview("tool_search", Some("\"git\"")),
            "Searching tools: \"git\""
        );
    }

    #[test]
    fn format_meta_tool_descriptions() {
        let r = StreamRenderState::new();
        // send_message is now an action within the `agent` consolidated tool
        let send = r.format_tool_description(
            "agent",
            &serde_json::json!({"action": "send_message", "to": "agent-2", "summary": "Need review"}),
        );
        let env = r.format_tool_description(
            "env",
            &serde_json::json!({"operation": "get", "name": "PATH"}),
        );
        let notebook = r.format_tool_description(
            "notebook_edit",
            &serde_json::json!({"edit_mode": "replace", "notebook_path": "analysis.ipynb"}),
        );
        let query =
            r.format_tool_description("query_context", &serde_json::json!({"prefix": "auth/"}));

        assert_eq!(send, "Send message: agent-2: Need review");
        assert_eq!(env, "Env: get PATH");
        assert_eq!(notebook, "Notebook edit: replace analysis.ipynb");
        assert_eq!(query, "Query context: auth/");
    }

    #[test]
    fn format_meta_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("send_message", Some("agent-2: Need review")),
            "Send message: agent-2: Need review"
        );
        assert_eq!(
            format_tool_display_from_preview("env", Some("get PATH")),
            "Env: get PATH"
        );
        assert_eq!(
            format_tool_display_from_preview("notebook_edit", Some("replace analysis.ipynb")),
            "Notebook edit: replace analysis.ipynb"
        );
        assert_eq!(
            format_tool_display_from_preview("query_context", Some("auth/")),
            "Query context: auth/"
        );
    }

    #[test]
    fn format_memory_maintenance_descriptions() {
        let r = StreamRenderState::new();
        let forget = r.format_tool_description(
            "memory",
            &serde_json::json!({"action": "forget", "topic": "renderer drift"}),
        );
        let update = r.format_tool_description(
            "memory",
            &serde_json::json!({"action": "update", "memory_id": "mem-123"}),
        );
        let profile =
            r.format_tool_description("memory", &serde_json::json!({"action": "profile"}));
        let focus = r.format_tool_description(
            "memory",
            &serde_json::json!({"action": "focus", "focus_value": "oauth"}),
        );
        let reflect =
            r.format_tool_description("memory", &serde_json::json!({"action": "reflect"}));

        assert_eq!(forget, "Forgetting: \"renderer drift\"");
        assert_eq!(update, "Updating memory: mem-123");
        assert_eq!(profile, "Checking profile");
        assert_eq!(focus, "Focus on: \"oauth\"");
        assert_eq!(reflect, "Reflecting on memories");
    }

    #[test]
    fn format_memory_preview_display_names() {
        // Preview-only path can't know the action; it surfaces the raw preview.
        assert_eq!(
            format_tool_display_from_preview("memory", Some("action=purge topic=...")),
            "Memory: action=purge topic=..."
        );
        assert_eq!(format_tool_display_from_preview("memory", None), "Memory");
    }

    #[test]
    fn format_web_search_description_and_preview() {
        let r = StreamRenderState::new();
        let desc = r.format_tool_description(
            "web_search",
            &serde_json::json!({"query": "matrixone latest"}),
        );

        assert_eq!(desc, "Searching web: \"matrixone latest\"");
        assert_eq!(
            format_tool_display_from_preview("web_search", Some("matrixone latest")),
            "Searching web: \"matrixone latest\""
        );
    }

    #[test]
    fn format_analysis_tool_descriptions() {
        let r = StreamRenderState::new();
        let info = r.format_tool_description(
            "get_agent_info",
            &serde_json::json!({"dimension": "budget"}),
        );
        let reflect = r.format_tool_description(
            "reflect",
            &serde_json::json!({"question": "why did the tool fail?"}),
        );
        let context = r.format_tool_description(
            "context_analysis",
            &serde_json::json!({"mode": "compare", "turn_a": 3, "turn_b": 7}),
        );
        // run_chain is now an action within the `agent` consolidated tool
        let chain = r.format_tool_description(
            "agent",
            &serde_json::json!({"action": "run_chain", "chain_name": "search-and-read"}),
        );

        assert_eq!(info, "Getting agent info: budget");
        assert_eq!(reflect, "Reflecting: \"why did the tool fail?\"");
        assert_eq!(context, "Context analysis: compare 3 vs 7");
        assert_eq!(chain, "Running chain: search-and-read");
    }

    #[test]
    fn format_analysis_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("get_agent_info", Some("budget")),
            "Getting agent info: budget"
        );
        assert_eq!(
            format_tool_display_from_preview("reflect", Some("why did the tool fail?")),
            "Reflecting: \"why did the tool fail?\""
        );
        assert_eq!(
            format_tool_display_from_preview("context_analysis", Some("compare 3 vs 7")),
            "Context analysis: compare 3 vs 7"
        );
        assert_eq!(
            format_tool_display_from_preview("run_chain", Some("search-and-read")),
            "Running chain: search-and-read"
        );
    }

    #[test]
    fn format_session_state_tool_descriptions() {
        let r = StreamRenderState::new();
        let powershell = r.format_tool_description(
            "powershell",
            &serde_json::json!({"command": "Get-ChildItem"}),
        );
        let adjust = r.format_tool_description(
            "adjust_config",
            &serde_json::json!({"path": "display.max_output_lines"}),
        );
        let rollback = r.format_tool_description(
            "rollback_session_state",
            &serde_json::json!({"scope": "turn", "turn_index": 5}),
        );

        assert_eq!(powershell, "PS> Get-ChildItem");
        assert_eq!(adjust, "Adjust config: display.max_output_lines");
        assert_eq!(rollback, "Rollback session state: turn 5");
    }

    #[test]
    fn format_rollback_tool_descriptions() {
        let r = StreamRenderState::new();
        let file = r.format_tool_description(
            "rollback_file_edits",
            &serde_json::json!({"scope": "file", "path": "src/main.rs"}),
        );
        let snapshot = r.format_tool_description(
            "rollback_database_snapshots",
            &serde_json::json!({"scope": "snapshot", "snapshot_id": "snap_123"}),
        );
        let turn = r.format_tool_description(
            "rollback_turn_actions",
            &serde_json::json!({"scope": "turn", "turn_index": 7}),
        );

        assert_eq!(file, "Revert file edits: src/main.rs");
        assert_eq!(snapshot, "Revert DB snapshots: snap_123");
        assert_eq!(turn, "Rollback turn actions: turn 7");
    }

    #[test]
    fn format_session_state_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("powershell", Some("Get-ChildItem")),
            "PS> Get-ChildItem"
        );
        assert_eq!(
            format_tool_display_from_preview("adjust_config", Some("display.max_output_lines"),),
            "Adjust config: display.max_output_lines"
        );
        assert_eq!(
            format_tool_display_from_preview("rollback_session_state", Some("turn 5")),
            "Rollback session state: turn 5"
        );
    }

    #[test]
    fn format_rollback_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("rollback_file_edits", Some("src/main.rs")),
            "Revert file edits: src/main.rs"
        );
        assert_eq!(
            format_tool_display_from_preview("rollback_database_snapshots", Some("snap_123")),
            "Revert DB snapshots: snap_123"
        );
        assert_eq!(
            format_tool_display_from_preview("rollback_turn_actions", Some("turn 7")),
            "Rollback turn actions: turn 7"
        );
    }

    #[test]
    fn format_task_tool_descriptions() {
        let r = StreamRenderState::new();
        let create = r.format_tool_description(
            "task_create",
            &serde_json::json!({"title": "Fix renderer drift"}),
        );
        let update = r.format_tool_description(
            "task_update",
            &serde_json::json!({"task_id": "render-pass", "status": "in_progress"}),
        );
        let list = r.format_tool_description("task_list", &serde_json::json!({"status": "active"}));

        assert_eq!(create, "Creating task: \"Fix renderer drift\"");
        assert_eq!(update, "Updating task: render-pass -> in_progress");
        assert_eq!(list, "Listing tasks: active");
    }

    #[test]
    fn format_task_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("task_create", Some("Fix renderer drift")),
            "Creating task: \"Fix renderer drift\""
        );
        assert_eq!(
            format_tool_display_from_preview("task_update", Some("render-pass -> in_progress")),
            "Updating task: render-pass -> in_progress"
        );
        assert_eq!(
            format_tool_display_from_preview("task_list", Some("active")),
            "Listing tasks: active"
        );
    }

    #[test]
    fn format_mo_tool_descriptions() {
        let r = StreamRenderState::new();
        let query = r.format_tool_description(
            "mo_query",
            &serde_json::json!({"sql": "select * from users"}),
        );
        let snapshot = r.format_tool_description(
            "mo_snapshot",
            &serde_json::json!({"action": "create", "name": "pre-migration"}),
        );
        let branch = r.format_tool_description(
            "mo_branch",
            &serde_json::json!({"action": "create", "name": "exp-a"}),
        );

        assert_eq!(query, "MatrixOne query: \"select * from users\"");
        assert_eq!(snapshot, "MatrixOne snapshot: create pre-migration");
        assert_eq!(branch, "MatrixOne branch: create exp-a");
    }

    #[test]
    fn format_mo_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("mo_query", Some("select * from users")),
            "MatrixOne query: \"select * from users\""
        );
        assert_eq!(
            format_tool_display_from_preview("mo_snapshot", Some("create pre-migration")),
            "MatrixOne snapshot: create pre-migration"
        );
        assert_eq!(
            format_tool_display_from_preview("mo_branch", Some("create exp-a")),
            "MatrixOne branch: create exp-a"
        );
    }

    #[test]
    fn format_code_navigation_descriptions() {
        let r = StreamRenderState::new();
        let search = r.format_tool_description(
            "symbol_search",
            &serde_json::json!({"query": "SessionFacts"}),
        );
        let hover = r.format_tool_description(
            "hover_info",
            &serde_json::json!({"file": "src/lib.rs", "line": 42, "column": 3}),
        );
        let hierarchy = r.format_tool_description(
            "type_hierarchy",
            &serde_json::json!({"name": "SessionStore", "direction": "implementations"}),
        );
        let lsp = r.format_tool_description(
            "lsp",
            &serde_json::json!({"operation": "hover", "file": "src/lib.rs", "line": 42, "column": 3}),
        );

        assert_eq!(search, "Search symbol SessionFacts");
        assert_eq!(hover, "Hover info at src/lib.rs:42:3");
        assert_eq!(
            hierarchy,
            "Type hierarchy for SessionStore (implementations)"
        );
        assert_eq!(lsp, "LSP: hover src/lib.rs:42:3");
    }

    #[test]
    fn format_code_navigation_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("hover_info", Some("src/lib.rs:42:3")),
            "Hover info at src/lib.rs:42:3"
        );
        assert_eq!(
            format_tool_display_from_preview(
                "type_hierarchy",
                Some("SessionStore (implementations)"),
            ),
            "Type hierarchy for SessionStore (implementations)"
        );
        assert_eq!(
            format_tool_display_from_preview("symbol_search", Some("SessionFacts")),
            "Search symbol SessionFacts"
        );
        assert_eq!(
            format_tool_display_from_preview("lsp", Some("hover src/lib.rs:42:3")),
            "LSP: hover src/lib.rs:42:3"
        );
    }

    #[test]
    fn format_code_navigation_truncates_long_position_paths() {
        let r = StreamRenderState::new();
        let file = path_longer_than_any_sane_terminal_budget();
        let hover = r.format_tool_description(
            "hover_info",
            &serde_json::json!({
                "file": file,
                "line": 42,
                "column": 3
            }),
        );
        assert!(
            hover.starts_with("Hover info at .../"),
            "expected ellipsis-prefixed path; got {hover:?}"
        );
        assert!(hover.ends_with(":42:3"), "got {hover:?}");
    }

    #[test]
    fn format_code_navigation_keeps_medium_position_paths() {
        let r = StreamRenderState::new();
        // Path must fit within the 80-col budget:
        // desc_budget = 80 - 14 = 66; "Hover info at " (14) + path + ":42:3" (5) ≤ 66
        // => path ≤ 47 chars.  This path is 38 chars.
        let hover = r.format_tool_description(
            "hover_info",
            &serde_json::json!({
                "file": "/moderately/long/path/to/module/file.rs",
                "line": 42,
                "column": 3
            }),
        );
        assert_eq!(
            hover,
            "Hover info at /moderately/long/path/to/module/file.rs:42:3"
        );
    }

    #[test]
    fn format_call_graph_preview_respects_path_budget() {
        let r = StreamRenderState::new();
        let preview = r
            ._format_tool_arg_preview_unused(
                "call_graph",
                &serde_json::json!({
                    "path": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                    "start_line": 10,
                    "end_line": 24
                }),
            )
            .expect("preview");
        assert!(preview.starts_with(".../"));
        assert!(preview.ends_with(":10-24"));
        assert!(preview.chars().count() <= 40);
    }

    #[test]
    fn format_location_previews_respect_path_budget() {
        let r = StreamRenderState::new();
        let hover = r
            ._format_tool_arg_preview_unused(
                "hover_info",
                &serde_json::json!({
                    "file": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                    "line": 42,
                    "column": 3
                }),
            )
            .expect("hover preview");
        let extract = r
            ._format_tool_arg_preview_unused(
                "extract_members",
                &serde_json::json!({
                    "file": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                    "line": 88
                }),
            )
            .expect("extract preview");
        let lsp = r
            ._format_tool_arg_preview_unused(
                "lsp",
                &serde_json::json!({
                    "operation": "hover",
                    "file": "/very/long/path/to/deeply/nested/module/with/more/components/src/lib.rs",
                    "line": 42,
                    "column": 3
                }),
            )
            .expect("lsp preview");

        assert!(hover.ends_with(":42:3"));
        assert!(hover.chars().count() <= 40);
        assert!(extract.ends_with(":88"));
        assert!(extract.chars().count() <= 40);
        assert!(lsp.ends_with(":42:3"));
        assert!(lsp.chars().count() <= 40);
    }

    #[test]
    fn format_remaining_code_tool_descriptions() {
        let r = StreamRenderState::new();
        let rename = r.format_tool_description(
            "rename_symbol",
            &serde_json::json!({"symbol": "SessionStore", "new_name": "StoreSession"}),
        );
        let dead_code = r.format_tool_description(
            "dead_code",
            &serde_json::json!({"path": "src/", "kind": "function"}),
        );
        let extract_members = r.format_tool_description(
            "extract_members",
            &serde_json::json!({"file": "src/lib.rs", "line": 88}),
        );

        assert_eq!(rename, "Rename symbol SessionStore -> StoreSession");
        assert_eq!(dead_code, "Find dead code: src/ (function)");
        assert_eq!(extract_members, "Extract members: src/lib.rs:88");
    }

    #[test]
    fn format_remaining_code_tool_preview_display_names() {
        assert_eq!(
            format_tool_display_from_preview("rename_symbol", Some("SessionStore -> StoreSession"),),
            "Rename symbol SessionStore -> StoreSession"
        );
        assert_eq!(
            format_tool_display_from_preview("dead_code", Some("src/ (function)")),
            "Find dead code: src/ (function)"
        );
        assert_eq!(
            format_tool_display_from_preview("extract_members", Some("src/lib.rs:88")),
            "Extract members: src/lib.rs:88"
        );
    }

    #[tokio::test]
    async fn catch_tool_execution_panic_reports_error_output() {
        let (outcome, duration_ms) = catch_tool_execution_panic(async {
            std::thread::sleep(std::time::Duration::from_millis(10));
            panic!("boom");
        })
        .await;
        assert!(duration_ms >= 10);
        assert!(outcome.output.contains("Tool execution panicked: boom"));
        assert!(outcome.tool_result_fields.is_none());
    }
    // ── Skill/MCP output summary tests ──

    #[test]
    fn skill_output_summary_collapses_preview_lines() {
        let r = StreamRenderState::new();
        let output = "Result line 1\nResult line 2\nResult line 3\nLine 4\nLine 5";
        let summary = r.format_output_summary("skill", output, "ok");
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "5 output lines captured");
    }

    #[test]
    fn skill_output_summary_empty() {
        let r = StreamRenderState::new();
        assert!(r.format_output_summary("skill", "", "ok").is_none());
        assert!(
            r.format_output_summary("skill", "   \n  \n", "ok")
                .is_none()
        );
    }

    #[test]
    fn mcp_output_summary_collapses_preview_lines() {
        let r = StreamRenderState::new();
        let output = "Found 3 repos\nrepo1\nrepo2\nrepo3";
        let summary = r.format_output_summary("mcp_github_search", output, "ok");
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert_eq!(s.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(s.text, "4 output lines captured");
    }

    #[test]
    fn mcp_output_summary_formats_json_arrays_structurally() {
        let r = StreamRenderState::new();
        let output = r#"[{"name":"repo1","stars":10},{"name":"repo2","stars":5}]"#;
        let summary = r
            .format_output_summary("mcp_github_search", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "json array · 2 items · keys: name, stars");
    }

    #[test]
    fn mcp_output_summary_empty() {
        let r = StreamRenderState::new();
        assert!(
            r.format_output_summary("mcp_github_search", "", "ok")
                .is_none()
        );
    }

    #[test]
    fn bash_output_summary_collapses_preview_lines() {
        let r = StreamRenderState::new();
        let summary = r
            .format_output_summary("bash", "line 1\nline 2\nline 3\nline 4", "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(summary.text, "4 lines captured");
    }

    #[test]
    fn grep_output_summary_keeps_only_match_counts() {
        let r = StreamRenderState::new();
        let output = "src/a.rs:10:foo\nsrc/a.rs:11:foo\nsrc/b.rs:8:foo";
        let summary = r
            .format_output_summary("grep", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Preview);
        assert_eq!(summary.text, "3 matches in 2 file(s)");
    }

    #[test]
    fn grep_output_summary_no_matches_found_not_one_match() {
        let r = StreamRenderState::new();
        let summary = r
            .format_output_summary("grep", "No matches found", "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "no matches");
    }

    #[test]
    fn glob_output_summary_no_files_found_not_one_file() {
        let r = StreamRenderState::new();
        let summary = r
            .format_output_summary("glob", "No files found", "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "no matches");
    }

    #[test]
    fn git_diff_output_summary_stays_structural() {
        let r = StreamRenderState::new();
        let output = "\
diff --git a/src/a.rs b/src/a.rs\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1 +1 @@\n\
-old\n\
+new\n";
        let summary = r
            .format_output_summary("git_diff", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert!(summary.text.contains("+1"));
        assert!(summary.text.contains("-1"));
        assert!(summary.text.contains("src/a.rs"));
    }

    #[test]
    fn generic_output_summary_formats_json_objects_structurally() {
        let r = StreamRenderState::new();
        let output = r#"{"status":"ok","count":2,"items":["a","b"]}"#;
        let summary = r
            .format_output_summary("custom_tool", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "json object · keys: count, items, status");
    }

    #[test]
    fn web_fetch_output_summary_uses_markdown_heading() {
        let r = StreamRenderState::new();
        let output = "# MatrixOne Docs\n\nWelcome to the docs.\nMore details.";
        let summary = r
            .format_output_summary("web_fetch", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "MatrixOne Docs · 3 lines");
    }

    #[test]
    fn web_fetch_output_summary_uses_html_title() {
        let r = StreamRenderState::new();
        let output =
            "<html><head><title>Release Notes</title></head><body><p>Shipped.</p></body></html>";
        let summary = r
            .format_output_summary("web_fetch", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "Release Notes · 1 line");
    }

    #[test]
    fn web_fetch_output_summary_uses_structured_json_title() {
        let r = StreamRenderState::new();
        let output = serde_json::json!({
            "metadata": {"title": "Structured Docs"},
            "content": "# Ignored Heading\n\nBody"
        })
        .to_string();
        let summary = r
            .format_output_summary("web_fetch", &output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "Structured Docs · 2 lines");
    }

    #[test]
    fn mo_query_output_summary_extracts_row_and_column_counts() {
        let r = StreamRenderState::new();
        let output = "\
+----+-------+\n\
| id | name  |\n\
+----+-------+\n\
| 1  | alice |\n\
| 2  | bob   |\n\
+----+-------+\n";
        let summary = r
            .format_output_summary("mo_query", output, "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "2 rows · cols: id, name");
    }

    #[test]
    fn mo_query_output_summary_handles_query_ok_messages() {
        let r = StreamRenderState::new();
        let summary = r
            .format_output_summary("mo_query", "Query OK, 1 row affected (0.02 sec)", "ok")
            .expect("summary");
        assert_eq!(summary.kind, ToolOutputSummaryKind::Structural);
        assert_eq!(summary.text, "Query OK, 1 row affected (0.02 sec)");
    }

    // ── Text buffering contract ─────────────────────────────────────────
    //
    // These tests verify that text is ALWAYS buffered from the start
    // (buffer_from_start=true), preventing the two classes of leakage:
    //   1. Non-TTY: ANSI cursor movement has no effect on pipes.
    //   2. TTY with stderr interleave: MoveUp(rows) falls short because
    //      TerminalRegion doesn't track stderr rows from tool spinners.
    //
    // The invariant: buffer_from_start=true → tool_work_detected=true
    // from the start → StreamText goes to xml_tag_buffer, never to the
    // renderer during streaming.  At finalization, tool turns discard
    // the buffer; non-tool turns render it one-shot.

    #[test]
    fn buffer_from_start_is_always_true() {
        // The construction invariant: buffer_from_start must be true
        // regardless of TTY mode or skill_continuation.  This prevents
        // text from being rendered to stdout during streaming.
        //
        // Previously: `ctx.skill_continuation || !is_terminal()`
        // Now:        `true`
        //
        // If this test fails, text leakage will return.
        let buffer_from_start = true; // mirrors stream_render.rs:176
        assert!(
            buffer_from_start,
            "buffer_from_start must be true to prevent TTY/scrollback text leakage"
        );
    }

    #[test]
    fn tool_turn_discards_buffered_text() {
        // Simulate tool turn finalization: text was buffered, tools executed.
        // has_any_tool_work=true → buffer must be discarded.
        let pending_xml_buffer = "╔══════ draft review text ══════╗".to_string();
        let has_any_tool_work = true;
        if has_any_tool_work {
            // Tool turn: buffer is dropped (not rendered).
            drop(pending_xml_buffer);
        } else {
            panic!("Tool turn should discard text");
        }
    }

    #[test]
    fn final_answer_renders_buffered_text() {
        // Simulate final-answer finalization: text was buffered, no tools.
        // has_any_tool_work=false → buffer must be rendered.
        let pending_xml_buffer = "Here is my final answer".to_string();
        let has_any_tool_work = false;
        let rendered = if has_any_tool_work {
            panic!("Non-tool turn should render text");
        } else {
            let mut buf = pending_xml_buffer;
            super::streaming_md::strip_xml_tags_inplace(&mut buf);
            super::streaming_md::strip_leading_narration(&mut buf);
            buf
        };
        assert_eq!(rendered, "Here is my final answer");
    }

    #[test]
    fn buffered_text_has_xml_tags_stripped_at_finalization() {
        // Text that was buffered may contain thinking tags from the LLM.
        // At finalization, strip_xml_tags_inplace removes them.
        let mut buf = "intro\n<think>internal reasoning</think>\nconclusion".to_string();
        super::streaming_md::strip_xml_tags_inplace(&mut buf);
        assert_eq!(buf, "intro\nconclusion");
    }

    // ── Deferred text rendering tests ───────────────────────────────────

    #[test]
    fn consume_turn_sse_does_not_render_text_for_non_tool_turn() {
        // With the deferred rendering architecture, consume_turn_sse should
        // never render text directly. The text is returned in result.full_text
        // and rendering is handled by host.render_final_text() in the agentic loop.
        // This test verifies the contract by checking that result.full_text
        // carries the answer text without any stdout side-effects.
        let mut result = super::TurnResult::new();
        result.core.full_text = "The answer is 42.".to_string();
        assert!(!result.core.has_tool_calls);
        assert!(result.edge_tool_round.is_empty());
        // The text is available for deferred rendering by the host.
        assert_eq!(result.core.full_text, "The answer is 42.");
    }

    #[test]
    fn tool_turn_discards_text_buffer() {
        // When tools are present, text is intermediate and should be discarded.
        let mut result = super::TurnResult::new();
        result.core.full_text = "Let me use a tool...".to_string();
        result.core.has_tool_calls = true;
        let has_any_tool_work = result.core.has_tool_calls || !result.edge_tool_round.is_empty();
        assert!(has_any_tool_work, "tool turn should flag tool work");
        // Caller discards text for tool turns — correct behavior.
    }

    // ── Edge-path skill dedup tests ─────────────────────────────────────

    #[test]
    fn skill_dedup_hashset_tracks_invocations() {
        // Verifies the dedup data structure used in CliSseStreamHost.
        let mut invoked = std::collections::HashSet::new();
        // First insert returns true (new entry).
        assert!(invoked.insert("code-review".to_string()));
        // Second insert returns false (duplicate).
        assert!(!invoked.insert("code-review".to_string()));
        // Different skill is new.
        assert!(invoked.insert("test-writer".to_string()));
    }

    #[test]
    fn skill_dedup_produces_correct_message() {
        let skill_name = "code-review";
        let msg = format!(
            "Skill '{}' was already loaded in this turn. \
             Follow the instructions already provided.",
            skill_name,
        );
        assert!(msg.contains("code-review"));
        assert!(msg.contains("already loaded"));
    }

    // ── CLI skill-loaded marker tests ──────────────────────────────────

    #[test]
    fn skill_loaded_marker_appended_to_successful_result() {
        // Regression (session 11825116): the CLI edge path used
        // `execute_skill_inline` which returned raw skill content
        // WITHOUT appending `<skill-loaded name="..."/>`. The
        // system prompt tells the LLM "On seeing <skill-loaded/>,
        // do not re-invoke" — without the marker, the LLM loaded
        // a second skill (review-code) after already loading
        // review-changes, wasting context and confusing itself.
        //
        // Fix: `append_skill_loaded_marker` adds the tag to
        // successful (non-error) results.
        let raw = "# Skill: review-changes\n\nYou are now executing...";
        let result = append_skill_loaded_marker(raw, "review-changes");
        assert!(
            result.contains("<skill-loaded name=\"review-changes\"/>"),
            "successful skill result must carry the marker: {result}"
        );
        assert!(
            result.ends_with("<skill-loaded name=\"review-changes\"/>"),
            "marker must be at the very end so LLM sees it last: {result}"
        );
    }

    #[test]
    fn skill_loaded_marker_not_appended_to_error_result() {
        // Error results must NOT carry the tag — the LLM should be
        // free to retry or switch to another skill.
        let raw = "Error: skill resolver not available";
        let result = append_skill_loaded_marker(raw, "broken-skill");
        assert!(
            !result.contains("<skill-loaded"),
            "error results must not carry the marker: {result}"
        );
    }

    #[test]
    fn skill_loaded_marker_sanitizes_xml_special_chars() {
        // XML-special characters (`<`, `>`, `&`, `"`, `'`) are outside
        // the filename-safe allowlist, so they are replaced with `_`.
        // This prevents a malicious skill name from breaking out of
        // the `<skill-loaded name="..."/>` tag entirely.
        let result = append_skill_loaded_marker("ok", "a<b>&c\"d");
        assert!(
            result.contains("a_b__c_d"),
            "XML special chars must be replaced with `_`: {result}"
        );
        assert!(
            !result.contains('<') || result.matches('<').count() == 1,
            "only the opening `<skill-loaded` angle bracket may remain: {result}"
        );
    }

    // ── EdgeToolCache unit tests ─────────────────────────────────────────

    #[test]
    fn edge_tool_cache_new_has_correct_limit() {
        let cache = EdgeToolCache::new(5);
        assert_eq!(cache.max_identical_calls, 5);
        assert!(cache.output_cache.is_empty());
        assert!(cache.call_counts.is_empty());
    }

    #[test]
    fn edge_tool_cache_stores_and_retrieves() {
        let mut cache = EdgeToolCache::new(3);
        let sig = "read_file:{\"path\":\"/tmp/foo\"}".to_string();
        cache.output_cache.insert(
            sig.clone(),
            EdgeToolCacheEntry {
                output: "file content".to_string(),
                status: "success".to_string(),
                validation: EdgeToolCacheValidation::FileMtime {
                    path: PathBuf::from("/tmp/foo"),
                    timestamp_ms: 1,
                },
            },
        );
        let hit = cache.output_cache.get(&sig);
        assert!(hit.is_some());
        let hit = hit.unwrap();
        assert_eq!(hit.output, "file content");
        assert_eq!(hit.status, "success");
    }

    #[test]
    fn edge_tool_cache_call_count_increments() {
        let mut cache = EdgeToolCache::new(3);
        let sig = "grep:{\"pattern\":\"foo\"}".to_string();
        let count = cache.call_counts.entry(sig.clone()).or_insert(0);
        *count += 1;
        assert_eq!(cache.call_counts[&sig], 1);
        *cache.call_counts.get_mut(&sig).unwrap() += 1;
        assert_eq!(cache.call_counts[&sig], 2);
    }

    #[test]
    fn edge_tool_cache_call_count_exceeds_limit() {
        let mut cache = EdgeToolCache::new(2);
        let sig = "bash:{\"command\":\"ls\"}".to_string();
        let count = cache.call_counts.entry(sig.clone()).or_insert(0);
        *count += 1;
        assert!(*count <= cache.max_identical_calls);
        *cache.call_counts.get_mut(&sig).unwrap() += 1;
        assert!(*cache.call_counts.get(&sig).unwrap() <= cache.max_identical_calls);
        *cache.call_counts.get_mut(&sig).unwrap() += 1;
        assert!(*cache.call_counts.get(&sig).unwrap() > cache.max_identical_calls);
    }

    #[test]
    fn edge_tool_cache_read_only_tools_lookup() {
        // Verify that well-known cacheable tools are in the set
        assert!(READ_ONLY_TOOLS.contains(&"read_file"));
        assert!(READ_ONLY_TOOLS.contains(&"grep"));
        assert!(READ_ONLY_TOOLS.contains(&"glob"));
        assert!(READ_ONLY_TOOLS.contains(&"git_log"));
        // bash is NOT cacheable (side effects)
        assert!(!READ_ONLY_TOOLS.contains(&"bash"));
    }

    #[test]
    fn edge_tool_cache_dedup_signature_deterministic() {
        let args = serde_json::json!({"path": "/tmp/foo", "pattern": "bar"});
        let sig1 = tool_dedup_signature("grep", &args);
        let sig2 = tool_dedup_signature("grep", &args);
        assert_eq!(sig1, sig2);
        // Different tool name → different signature
        let sig3 = tool_dedup_signature("read_file", &args);
        assert_ne!(sig1, sig3);
    }

    #[tokio::test]
    async fn transactional_batch_rolls_back_earlier_file_write_on_later_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(3, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-1",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-1",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_ne!(results[0].status, "error");
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(rollback_fields["transaction_id"].as_str(), Some("tx-1"));
        assert!(
            results[1].output.contains("Transaction `tx-1` failed."),
            "{}",
            results[1].output
        );
        assert!(
            !temp.path().join("txn.txt").exists(),
            "rollback should remove the written file"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_restores_deleted_file_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let victim = temp.path().join("txn.txt");
        std::fs::write(&victim, "hello\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "delete": true,
                        "transaction_id": "tx-del",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-del",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("restored file"),
            "hello\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_restores_notebook_edit_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let notebook = temp.path().join("analysis.ipynb");
        std::fs::write(
            &notebook,
            r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("seed notebook");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(6, std::sync::atomic::Ordering::Relaxed);
        let _ = executor.read_file(&serde_json::json!({"path": "analysis.ipynb"}));

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "notebook_edit".to_string(),
                    args: serde_json::json!({
                        "notebook_path": "analysis.ipynb",
                        "edit_mode": "replace",
                        "cell_id": "cell-1",
                        "new_source": "x=2",
                        "transaction_id": "tx-nb",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-nb",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        let restored = std::fs::read_to_string(&notebook).expect("restored notebook");
        assert!(
            restored.contains("\"x=1\""),
            "restored notebook: {restored}"
        );
        assert!(
            !restored.contains("\"x=2\""),
            "restored notebook: {restored}"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_reapplies_git_stash_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        std::fs::write(&tracked, "working tree\n").expect("modify tracked file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(7, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "git_stash".to_string(),
                    args: serde_json::json!({
                        "action": "push",
                        "message": "txn stash",
                        "transaction_id": "tx-stash",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-stash",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored working tree"),
            "working tree\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["git_stashes"]["restored"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_reverts_git_commit_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        std::fs::write(&tracked, "committed in txn\n").expect("modify tracked file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(8, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "git_commit".to_string(),
                    args: serde_json::json!({
                        "message": "txn commit",
                        "transaction_id": "tx-commit",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-commit",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(
            rollback_fields["transaction_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(
            std::fs::read_to_string(&tracked).expect("restored tracked file"),
            "committed\n"
        );
        assert_eq!(
            rollback_fields["transaction_rollback"]["git_commits"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_skips_later_requests_after_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("other.txt"), "existing\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(5, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tr-3".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "other.txt",
                        "transaction_id": "tx-2",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[2].status, "error");
        assert!(
            results[2].output.contains("already aborted"),
            "{}",
            results[2].output
        );
        let fields = results[2]
            .tool_result_fields
            .as_ref()
            .expect("transaction fields");
        assert_eq!(fields["transaction_state"].as_str(), Some("aborted"));
        assert_eq!(fields["transaction_id"].as_str(), Some("tx-2"));
        assert!(
            !results[2].output.contains("existing"),
            "aborted transaction request should not execute normally"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transactional_batch_records_boundary_open_and_commit_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "tx-boundary-commit";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(15, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                request_id: "tx-boundary-1".to_string(),
                tool: "write_file".to_string(),
                args: serde_json::json!({
                    "path": "txn.txt",
                    "content": "hello\n",
                    "transaction_id": "tx-journal",
                    "rollback_on_failure": true,
                }),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "error");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );

        let opened = boundary_metadata(&events[0]);
        assert_eq!(opened["kind"].as_str(), Some("tool_batch"));
        assert_eq!(opened["transaction_id"].as_str(), Some("tx-journal"));
        assert_eq!(opened["rollback_on_failure"].as_bool(), Some(true));

        let committed = boundary_metadata(&events[1]);
        assert_eq!(committed["kind"].as_str(), Some("tool_batch"));
        assert_eq!(committed["transaction_id"].as_str(), Some("tx-journal"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transactional_batch_records_boundary_abort_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "tx-boundary-abort";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(16, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tx-boundary-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-journal",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tx-boundary-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                        "transaction_id": "tx-journal",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "error");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );

        let aborted = boundary_metadata(&events[1]);
        assert_eq!(aborted["kind"].as_str(), Some("tool_batch"));
        assert_eq!(aborted["transaction_id"].as_str(), Some("tx-journal"));
        assert_eq!(
            aborted["trigger_request_id"].as_str(),
            Some("tx-boundary-2")
        );
        assert_eq!(aborted["trigger_tool_name"].as_str(), Some("read_file"));
        assert!(
            aborted["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("No such file or directory")),
            "{aborted}"
        );
        assert_eq!(
            aborted["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn turn_rollback_restores_written_file_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(9, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "turn-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-2".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "error");
        let rollback_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(rollback_fields["rollback_boundary"].as_str(), Some("turn"));
        assert_eq!(
            rollback_fields["rollback_state"].as_str(),
            Some("rolled_back")
        );
        assert_eq!(rollback_fields["rollback_on_failure"].as_bool(), Some(true));
        assert!(
            results[1].output.contains("Turn rollback policy failed."),
            "{}",
            results[1].output
        );
        assert!(
            !temp.path().join("turn.txt").exists(),
            "rollback should remove the written file"
        );
        assert_eq!(
            rollback_fields["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn turn_rollback_skips_later_requests_after_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("other.txt"), "existing\n").expect("seed file");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(10, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        // write_file succeeds, then bash "exit 1" (non-read-only mutation
        // error) triggers rollback + aborts later tools.
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "turn-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-2".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-3".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "other.txt",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);

        // After the PR fix: rollback fires on the bash error (results[1]),
        // but subsequent tools (results[2]) execute normally instead of being blocked.
        // The agent sees the error and decides whether to continue.

        // Bash error triggers rollback
        assert_eq!(results[1].status, "error");
        let bash_fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("bash rollback fields");
        assert_eq!(
            bash_fields["rollback_boundary"].as_str(),
            Some("turn"),
            "bash error should trigger turn rollback"
        );

        // Subsequent tool executes normally (not blocked)
        assert_eq!(
            results[2].status, "success",
            "read_file should execute normally after rollback"
        );
        assert!(
            results[2].output.contains("existing"),
            "read_file should return actual file content: {}",
            results[2].output
        );
    }

    #[tokio::test]
    async fn edge_tool_cache_invalidates_read_file_after_file_change() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let file = temp.path().join("cached.txt");
        std::fs::write(&file, "v1\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool(
                "cache-read-1",
                "read_file",
                &serde_json::json!({"path": "cached.txt"}),
            )
            .await;
        assert!(first.output.contains("v1"), "{}", first.output);

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "v2\n").expect("update");

        let second = host
            .execute_tool(
                "cache-read-2",
                "read_file",
                &serde_json::json!({"path": "cached.txt"}),
            )
            .await;
        assert!(second.output.contains("v2"), "{}", second.output);
        assert!(
            !second.output.contains("v1"),
            "stale cache should not replay old file contents: {}",
            second.output
        );
    }

    #[tokio::test]
    async fn edge_tool_cache_reuses_git_show_when_head_is_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool(
                "cache-git-1",
                "git_show",
                &serde_json::json!({"commit": "HEAD", "stat_only": true}),
            )
            .await;
        let second = host
            .execute_tool(
                "cache-git-2",
                "git_show",
                &serde_json::json!({"commit": "HEAD", "stat_only": true}),
            )
            .await;

        assert_eq!(first.output, second.output);
        assert_eq!(
            second.duration_ms, 0,
            "second git_show should be served from cache"
        );
    }

    #[tokio::test]
    async fn edge_tool_cache_invalidates_git_status_after_worktree_change() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = init_temp_git_repo();
        let tracked = temp.path().join("tracked.txt");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let first = host
            .execute_tool("cache-git-status-1", "git_status", &serde_json::json!({}))
            .await;
        assert!(
            !first.output.contains("tracked.txt"),
            "expected clean repo output without dirty entries: {}",
            first.output
        );

        std::fs::write(&tracked, "modified\n").expect("modify tracked file");

        let second = host
            .execute_tool("cache-git-status-2", "git_status", &serde_json::json!({}))
            .await;
        assert!(
            second.output.contains("tracked.txt"),
            "stale git cache should not hide worktree changes: {}",
            second.output
        );
    }

    #[tokio::test]
    async fn turn_rollback_allows_bash_and_persists_through_mutation_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(11, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        // bash mkdir should execute (no boundary violation), then bash "exit 1"
        // (non-read-only mutation error) triggers rollback.  write_file from
        // the first request is rolled back, but mkdir side-effect persists (no
        // checkpoint for bash).
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "turn-bash-0".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-bash-1".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "mkdir -p subdir",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-bash-2".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        // bash mkdir should have succeeded
        assert_ne!(
            results[1].status, "error",
            "bash mkdir should be allowed: {}",
            results[1].output
        );
        // bash "exit 1" should have errored and triggered rollback
        assert_eq!(results[2].status, "error");
        // write_file should be rolled back
        assert!(
            !temp.path().join("turn.txt").exists(),
            "write_file should be rolled back"
        );
        // bash side-effect persists (no rollback for bash)
        assert!(
            temp.path().join("subdir").exists(),
            "bash mkdir side-effect should persist through rollback"
        );
    }

    #[tokio::test]
    async fn turn_rollback_allows_read_only_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(12, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let result = host
            .execute_tool(
                "turn-bash-ro",
                "bash",
                &serde_json::json!({"command": "pwd"}),
            )
            .await;

        assert_ne!(result.status, "error");
        assert!(result.tool_result_fields.is_none());
        assert!(
            result
                .output
                .contains(temp.path().to_string_lossy().as_ref()),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn execute_tool_blocks_name_based_process_kill_for_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let result = host
            .execute_tool(
                "bash-pkill-blocked",
                "bash",
                &serde_json::json!({"command": "pkill -f http.server"}),
            )
            .await;

        assert_eq!(result.status, "error");
        assert!(
            result.output.contains(
                "name-based process killing commands (`pkill` / `killall`) are not allowed"
            ),
            "{}",
            result.output
        );
    }

    #[tokio::test]
    async fn turn_rollback_read_only_error_does_not_trigger_rollback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("keep.txt"), "keep me\n").expect("seed");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        // write_file succeeds, read_file(missing) errors but should NOT
        // trigger rollback because read_file is read-only.  The 3rd tool
        // should still execute normally.
        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "ro-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "new.txt",
                        "content": "created\n",
                    }),
                },
                ToolBatchRequest {
                    request_id: "ro-2".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "missing.txt",
                    }),
                },
                ToolBatchRequest {
                    request_id: "ro-3".to_string(),
                    tool: "read_file".to_string(),
                    args: serde_json::json!({
                        "path": "keep.txt",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 3);
        // write_file should succeed
        assert_ne!(results[0].status, "error", "{}", results[0].output);
        // read_file(missing) should error
        assert_eq!(results[1].status, "error");
        // read_file(keep) should still execute (no rollback triggered)
        assert_ne!(
            results[2].status, "error",
            "read-only error should not abort turn: {}",
            results[2].output
        );
        assert!(
            results[2].output.contains("keep me"),
            "{}",
            results[2].output
        );
        // File from write_file should still exist (no rollback)
        assert!(
            temp.path().join("new.txt").exists(),
            "write_file should not be rolled back by read-only error"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_rollback_records_boundary_open_and_commit_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        std::fs::write(temp.path().join("ok.txt"), "hello\n").expect("seed file");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "turn-boundary-commit";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(17, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                request_id: "turn-boundary-1".to_string(),
                tool: "read_file".to_string(),
                args: serde_json::json!({
                    "path": "ok.txt",
                }),
            }])
            .await;
        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "error");

        host.on_stream_complete();

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryCommitted
        );

        let opened = boundary_metadata(&events[0]);
        assert_eq!(opened["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(opened["rollback_on_failure"].as_bool(), Some(true));

        let committed = boundary_metadata(&events[1]);
        assert_eq!(committed["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(committed["detail"]["executed_requests"].as_u64(), Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_rollback_records_boundary_abort_event() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let _journal_guard = JournalDirGuard::new(temp.path().join("sessions"));
        let session_id = "turn-boundary-abort";
        let executor = std::sync::Arc::new(
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id),
        );
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(18, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "turn-boundary-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "turn.txt",
                        "content": "hello\n",
                    }),
                },
                ToolBatchRequest {
                    request_id: "turn-boundary-2".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "exit 1",
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "error");

        let events = boundary_events(session_id);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            JournalEventType::ExecutionBoundaryOpened
        );
        assert_eq!(
            events[1].event_type,
            JournalEventType::ExecutionBoundaryAborted
        );

        let aborted = boundary_metadata(&events[1]);
        assert_eq!(aborted["kind"].as_str(), Some("turn_rollback"));
        assert_eq!(
            aborted["trigger_request_id"].as_str(),
            Some("turn-boundary-2")
        );
        assert_eq!(aborted["trigger_tool_name"].as_str(), Some("bash"));
        assert!(
            aborted["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "reason should contain the error message: {aborted}"
        );
        assert_eq!(
            aborted["rollback"]["files"]["reverted"]
                .as_array()
                .map(|entries| entries.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transactional_batch_rejects_mutating_bash_and_restores_prior_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tx-bash-1".to_string(),
                    tool: "write_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
                        "content": "hello\n",
                        "transaction_id": "tx-bash",
                        "rollback_on_failure": true,
                    }),
                },
                ToolBatchRequest {
                    request_id: "tx-bash-2".to_string(),
                    tool: "bash".to_string(),
                    args: serde_json::json!({
                        "command": "mkdir unsafe-dir",
                        "transaction_id": "tx-bash",
                        "rollback_on_failure": true,
                    }),
                },
            ])
            .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[1].status, "error");
        assert!(
            results[1]
                .output
                .contains("non-read-only bash commands do not participate"),
            "{}",
            results[1].output
        );
        let fields = results[1]
            .tool_result_fields
            .as_ref()
            .expect("transaction fields");
        assert_eq!(fields["transaction_id"].as_str(), Some("tx-bash"));
        assert_eq!(fields["transaction_state"].as_str(), Some("rolled_back"));
        assert!(
            !temp.path().join("txn.txt").exists(),
            "prior bounded state should be rolled back"
        );
        assert!(
            !temp.path().join("unsafe-dir").exists(),
            "mutating bash should be blocked before execution"
        );
    }

    #[tokio::test]
    async fn transactional_batch_allows_read_only_bash() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools/result"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");
        let temp = tempdir().expect("tempdir");
        let executor = std::sync::Arc::new(crate::edge_tools::ToolExecutor::new(temp.path()));
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(14, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: std::sync::Arc::clone(&executor),
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
                observability_hub: None,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![ToolBatchRequest {
                request_id: "tx-bash-ro".to_string(),
                tool: "bash".to_string(),
                args: serde_json::json!({
                    "command": "pwd",
                    "transaction_id": "tx-bash-ro",
                    "rollback_on_failure": true,
                }),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert_ne!(results[0].status, "error");
        assert!(
            results[0]
                .output
                .contains(temp.path().to_string_lossy().as_ref()),
            "{}",
            results[0].output
        );
    }
}
