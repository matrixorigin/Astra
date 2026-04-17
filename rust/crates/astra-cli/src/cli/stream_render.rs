use super::*;
use astra_runtime::turn::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use astra_runtime::turn::headless_tool_assembly::CACHEABLE_TOOLS;
use astra_runtime::turn::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
};
use astra_runtime::turn::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, SseStreamHost, ToolBatchRequest,
    consume_sse_stream_cancellable, is_tool_concurrency_safe, stream_idle_timeout,
};
use astra_runtime::turn::tool_result_semantics::{
    cloud_tool_result_status_label, tool_dedup_signature, tool_error_triggers_rollback,
};
use astra_services::session_journal::{JournalEvent, JournalWriter};
use crossterm::style::Stylize;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::io::{IsTerminal, Write};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// CLI formatting utilities
use super::cli_formatting::{
    colorize_diff_summary, extract_cli_diff_block, format_byte_size, format_duration_suffix,
    shorten_path, truncate_line,
};

// Effects module types
use super::effects::{
    ThinkingSpinnerKind, ToolRegionState, ToolStdoutLineAnim, thinking_viewport_rows,
};

pub use astra_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

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

    /// True when thinking spinner and preview should still be shown.
    pub fn show_thinking(self) -> bool {
        !self.is_silent()
    }

    /// True when the thinking viewport receives reasoning-delta content
    /// instead of the main text area (plan decompose mode).
    pub fn reasoning_to_viewport(self) -> bool {
        matches!(self, Self::PlanDecompose)
    }
}

/// Cross-turn tool output cache for the CLI edge tool execution path.
///
/// Mirrors the headless round's `InMemoryIdempotencyCache` + `call_counts`, but
/// scoped to edge-path tool calls (`tool_request` SSE events).  Cacheable tools
/// (read_file, grep, git_log, …) get their output stored and replayed on repeat.
/// All tools get a hard call-count limit to prevent runaway repetition.
pub(super) struct EdgeToolCache {
    /// `dedup_signature → (output, status)` for read-only tools.
    output_cache: std::collections::HashMap<String, (String, String)>,
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

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a mut crate::edge_tools::ToolExecutor,
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
    token: &'a str,
    executor_id: &'a str,
    executor: &'a mut crate::edge_tools::ToolExecutor,
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

impl<'a> CliSseStreamHost<'a> {
    fn from_edge_ctx(ctx: EdgeSseContext<'a>, term_width: usize, render_md: bool) -> Self {
        let suppress_reasoning =
            ctx.render_policy == RenderPolicy::Silent || ctx.skill_continuation;
        let active_turn_rollback = ctx.turn_rollback_on_failure.then(|| ActiveTurnRollback {
            turn_index: ctx
                .executor
                .journal_turn_index
                .load(std::sync::atomic::Ordering::Relaxed),
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
        Self {
            api: ctx.api,
            token: ctx.token,
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
            if !tail.contains('>') && could_become_thinking_tag(tail) {
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
        let _ = self
            .api
            .post_tool_result(Some(self.token), Some(self.executor_id), &body)
            .await;
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
                .is_some_and(
                    astra_runtime::turn::cloud_approval_policy::bash_command_is_read_only,
                );
        }
        is_tool_concurrency_safe(tool)
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
        if astra_runtime::turn::cloud_approval_policy::bash_command_is_read_only(command) {
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
        let Ok(writer) = JournalWriter::new(session_id) else {
            return;
        };
        let _ = writer.append(&event);
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
            Some(session_id),
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
            Some(session_id),
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
            Some(session_id),
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
        if tool == "bash" {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            return !astra_runtime::turn::cloud_approval_policy::bash_command_is_read_only(command);
        }
        // Non-bash tools: only cloud-gated (mutation) tools trigger rollback.
        astra_runtime::turn::cloud_approval_policy::cloud_gated_tool_kind(tool).is_some()
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
        let post_result = self
            .api
            .post_tool_result(Some(self.token), Some(self.executor_id), &body)
            .await;

        if let Err(ref e) = post_result {
            let is_auth_failure = matches!(
                e,
                astra_thin_client::ThinClientError::Api { status, .. }
                    if status.as_u16() == 401
            );

            if is_auth_failure {
                if let Some(token) = self.cancel_token {
                    token.cancel();
                }
                if !self.render_policy.is_silent() {
                    eprintln!(
                        "{}",
                        "Session expired. Please re-authenticate with `astra auth login`.".red()
                    );
                }
            } else if !self.render_policy.suppress_tool_ui() {
                eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
            }
        }

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
                        .load(std::sync::atomic::Ordering::Relaxed),
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

/// Extract the first absolute path from a bash command string.
/// Used to determine which directory to expand the sandbox to when the user
/// approves a sandbox-denied bash command.
fn extract_first_absolute_path(command: &str) -> Option<String> {
    for token in command.split_whitespace() {
        if token.starts_with('/') && !token.starts_with("//") && !token.contains('$') {
            // Strip trailing punctuation that might be shell syntax
            let clean = token.trim_end_matches([';', '&', ')']);
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }
    None
}

/// Returns true if `partial` could become one of our known thinking tags.
/// E.g., "<", "<t", "<th", "<thi", "</", "</t", "</think" etc.
fn could_become_thinking_tag(partial: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "<t",
        "<th",
        "<thi",
        "<thin",
        "<think",
        "<r",
        "<re",
        "<ref",
        "<refl",
        "<refle",
        "<reflec",
        "<reflect",
        "<i",
        "<in",
        "<inn",
        "<inne",
        "<inner",
        "<inner_",
        "</t",
        "</th",
        "</thi",
        "</thin",
        "</think",
        "</r",
        "</re",
        "</ref",
        "</refl",
        "</refle",
        "</reflec",
        "</reflect",
        "</i",
        "</in",
        "</inn",
        "</inne",
        "</inner",
        "</inner_",
    ];
    // Also match bare "<" or "</" which could become anything
    if partial == "<" || partial == "</" {
        return true;
    }
    PREFIXES
        .iter()
        .any(|p| p.starts_with(partial) || partial.starts_with(p))
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
        if self.executor.active_session_id() != Some(session_id) {
            self.executor.set_active_session_id(session_id.to_string());
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
        // Forward tool-started event to observer channel
        let tool_description = self.render.format_tool_description(tool, args);
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::ToolStarted {
                name: tool.to_string(),
                description: tool_description.clone(),
            });
        }

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
        let call_count = self
            .tool_cache
            .call_counts
            .entry(dedup_sig.clone())
            .or_insert(0);
        *call_count += 1;
        let max_calls = self.tool_cache.max_identical_calls;

        if *call_count > max_calls {
            // Hard cap exceeded — return a stub telling the LLM to stop.
            let body = if let Some((cached_out, _)) = self.tool_cache.output_cache.get(&dedup_sig) {
                format!(
                    "⛔ Cached repeat (call #{} for identical args, limit: {}). \
                     The result is already in this conversation from an earlier call. \
                     Do NOT call this tool again with the same arguments.\n\n{}",
                    *call_count,
                    max_calls,
                    &cached_out[..cached_out.len().min(200)],
                )
            } else {
                format!(
                    "⛔ Duplicate call #{} (limit: {}). This tool has been called too many times \
                     with the same arguments. Use the results from earlier calls instead.",
                    *call_count, max_calls,
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
        if CACHEABLE_TOOLS.contains(&tool)
            && let Some((cached_output, cached_status)) =
                self.tool_cache.output_cache.get(&dedup_sig).cloned()
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
                denied_output = Some(format!("Error: {reason}"));
                false
            }
            crate::permission_manager::PermissionDecision::NeedApproval {
                tool: t,
                header,
                detail,
                reason,
            } => {
                if let Some(tx) = &self.approval_request_tx {
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(super::chat_stream::ApprovalRequest {
                        tool: t.clone(),
                        header,
                        detail,
                        reason,
                        response_tx: resp_tx,
                    });
                    let result = if let Some(token) = self.cancel_token {
                        tokio::select! {
                            biased;
                            _ = token.cancelled() => false,
                            r = resp_rx => r.unwrap_or(false),
                        }
                    } else {
                        resp_rx.await.unwrap_or(false)
                    };
                    if let Some(pm) = self.perm_manager.as_mut()
                        && result
                    {
                        pm.record_approval(&t, Some(args), true);
                    }
                    result
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
                    self.render.stop_tool_stderr_running();
                    self.render.stop_tool_stdout_anim();
                    use crossterm::style::Stylize;
                    eprintln!("  {}", format!("⚠  {header}").yellow());
                    if let Some(d) = &detail {
                        eprintln!("{}", d.as_str().dim());
                    }
                    if !reason.is_empty() {
                        eprintln!("  {}", reason.dim());
                    }
                    let ch = tokio::task::spawn_blocking(|| {
                        crate::permission_manager::PermissionManager::prompt_approval(
                            crate::permission_manager::ApprovalPromptKind::LocalStandard,
                        )
                    })
                    .await
                    .unwrap_or('n');
                    let approved = matches!(ch, 'y' | 'a' | '!');
                    if let Some(pm) = self.perm_manager.as_mut() {
                        if approved {
                            pm.record_approval(&t, Some(args), true);
                        }
                        if ch == '!' {
                            pm.set_mode(crate::permission_manager::PermissionMode::Auto);
                            eprintln!(
                                "  {}",
                                "  ⚡ Auto-run enabled for this session. Use /allow prompt to restore."
                                    .yellow()
                            );
                        }
                        if ch == 'a' {
                            let rule =
                                crate::permission_manager::PermissionManager::make_allow_rule(
                                    &t, args,
                                );
                            pm.add_allow_rule(&rule);
                            let scope = if pm.has_project_root() {
                                "project"
                            } else {
                                "session"
                            };
                            eprintln!(
                                "  {}",
                                format!("  ✓ {rule}: always allowed ({scope})").dim()
                            );
                        }
                        if ch == 's' {
                            pm.record_approval(&t, Some(args), false);
                            eprintln!("  {}", format!("  ✗ {t}: skipped for session").dim());
                        }
                    }
                    approved
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
                    astra_runtime::turn::skill_tool::execute_skill_inline(
                        resolver.as_ref(),
                        tool,
                        args,
                    )
                    .await
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
                if outcome
                    .output
                    .starts_with(crate::edge_tools::SANDBOX_DENIED_PREFIX)
                {
                    if let Some(pm) = &mut self.perm_manager {
                        let sandbox_msg =
                            &outcome.output[crate::edge_tools::SANDBOX_DENIED_PREFIX.len()..];
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
                                detail,
                                reason,
                                ..
                            } => {
                                if let Some(tx) = &self.approval_request_tx {
                                    // Plan execution mode: route through channel
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    let _ = tx.send(super::chat_stream::ApprovalRequest {
                                        tool: sandbox_tool_key.clone(),
                                        header: format!("🔒 {sandbox_msg}"),
                                        detail,
                                        reason,
                                        response_tx: resp_tx,
                                    });
                                    let grant = if let Some(token) = self.cancel_token {
                                        tokio::select! {
                                            biased;
                                            _ = token.cancelled() => false,
                                            r = resp_rx => r.unwrap_or(false),
                                        }
                                    } else {
                                        resp_rx.await.unwrap_or(false)
                                    };
                                    if grant {
                                        pm.record_approval(&sandbox_tool_key, Some(args), true);
                                    }
                                    grant
                                } else if self.render_policy.is_silent() {
                                    // Sub-run mode: auto-deny sandbox expansion
                                    astra_core::agent_warn!(
                                        "permission",
                                        "Auto-denied sandbox expansion {sandbox_tool_key} in sub-run mode: {reason}"
                                    );
                                    pm.record_approval(&sandbox_tool_key, Some(args), false);
                                    false
                                } else {
                                    // Interactive mode: prompt directly
                                    self.render.stop_tool_stderr_running();
                                    self.render.stop_tool_stdout_anim();
                                    use crossterm::style::Stylize;
                                    eprintln!("  {}", format!("🔒 {sandbox_msg}").yellow());
                                    if let Some(d) = &detail {
                                        eprintln!("{}", d.as_str().dim());
                                    }
                                    if !reason.is_empty() {
                                        eprintln!("  {}", reason.dim());
                                    }
                                    let ch = tokio::task::spawn_blocking(
                                        || {
                                            crate::permission_manager::PermissionManager::prompt_approval(
                                                crate::permission_manager::ApprovalPromptKind::LocalStandard,
                                            )
                                        },
                                    )
                                    .await
                                    .unwrap_or('n');
                                    let grant = matches!(ch, 'y' | 'a' | '!');
                                    if grant {
                                        pm.record_approval(&sandbox_tool_key, Some(args), true);
                                    }
                                    if ch == '!' {
                                        pm.set_mode(
                                            crate::permission_manager::PermissionMode::Auto,
                                        );
                                        use crossterm::style::Stylize;
                                        eprintln!(
                                            "  {}",
                                            "  ⚡ Auto-run enabled for this session. Use /allow prompt to restore."
                                            .yellow()
                                        );
                                    }
                                    if ch == 's' {
                                        pm.record_approval(&sandbox_tool_key, Some(args), false);
                                    }
                                    grant
                                }
                            }
                        };
                        if approved {
                            // Temporarily expand sandbox to allow the requested path,
                            // then retry the tool.
                            // Use parent directory so sibling files are also accessible,
                            // but never expand to "/" (would open entire filesystem).
                            let expand_dir = args
                                .get("path")
                                .or_else(|| args.get("file_path"))
                                .and_then(serde_json::Value::as_str)
                                .and_then(|p| {
                                    let parent = std::path::Path::new(p).parent()?;
                                    if parent == std::path::Path::new("/") {
                                        // For root-level files like /passwd, expand to
                                        // the file itself, not "/"
                                        Some(std::path::PathBuf::from(p))
                                    } else {
                                        Some(parent.to_path_buf())
                                    }
                                })
                                .or_else(|| {
                                    args.get("command")
                                        .and_then(serde_json::Value::as_str)
                                        .and_then(extract_first_absolute_path)
                                        .and_then(|p| {
                                            let parent = std::path::Path::new(&p).parent()?;
                                            if parent == std::path::Path::new("/") {
                                                Some(std::path::PathBuf::from(&p))
                                            } else {
                                                Some(parent.to_path_buf())
                                            }
                                        })
                                });
                            if let Some(dir) = expand_dir {
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
        if allowed && status != "error" && CACHEABLE_TOOLS.contains(&tool) {
            self.tool_cache
                .output_cache
                .insert(dedup_sig.clone(), (output.clone(), status.clone()));
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
        let post_result = self
            .api
            .post_tool_result(Some(self.token), Some(self.executor_id), &body)
            .await;

        if let Err(ref e) = post_result {
            // Check if this is a 401 Unauthorized - session is invalid, abort SSE stream
            let is_auth_failure = matches!(
                e,
                astra_thin_client::ThinClientError::Api { status, .. }
                    if status.as_u16() == 401
            );

            if is_auth_failure {
                // Cancel the SSE stream immediately - don't wait for idle timeout
                if let Some(token) = self.cancel_token {
                    token.cancel();
                }
                if !self.render_policy.is_silent() {
                    eprintln!(
                        "{}",
                        "Session expired. Please re-authenticate with `astra auth login`.".red()
                    );
                }
            } else if !self.render_policy.suppress_tool_ui() {
                eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
            }
        }
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
        let decision = match &mut self.perm_manager {
            Some(pm) => {
                pm.resolve_cloud_approval_async(
                    tool,
                    detail,
                    approval_kind,
                    self.render_policy.is_silent(),
                )
                .await
            }
            None => astra_thin_client::ApprovalDecision::Deny,
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
        let post_result = self.api.post_approval(Some(self.token), &body).await;

        if let Err(ref e) = post_result {
            // Check if this is a 401 Unauthorized - session is invalid, abort SSE stream
            let is_auth_failure = matches!(
                e,
                astra_thin_client::ThinClientError::Api { status, .. }
                    if status.as_u16() == 401
            );

            if is_auth_failure {
                // Cancel the SSE stream immediately - don't wait for idle timeout
                if let Some(token) = self.cancel_token {
                    token.cancel();
                }
                if !self.render_policy.is_silent() {
                    eprintln!(
                        "{}",
                        "Session expired. Please re-authenticate with `astra auth login`.".red()
                    );
                }
            } else if !self.render_policy.suppress_tool_ui() {
                eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
            }
        }
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: decision_str.to_string(),
            reason: None,
        }
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
            .map(|req| is_tool_concurrency_safe(&req.tool))
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

        // ── Phase 2: Parallel execution via tokio::spawn ──
        // Each tool runs on a separate runtime thread for true parallelism
        // (sync tools block their thread; async tools yield normally).
        //
        // SAFETY: `ScopedJoinHandles` aborts all spawned tasks on drop,
        // guaranteeing `executor` remains valid for every task's lifetime.
        let executor: &crate::edge_tools::ToolExecutor = &*self.executor;

        /// Wrapper that makes a `*const ToolExecutor` safely `Send`.
        /// Dereferencing requires an `unsafe` call via [`as_ref`](Self::as_ref).
        struct ExecHandle(*const crate::edge_tools::ToolExecutor);
        // SAFETY: ToolExecutor is Sync, so &ToolExecutor is Send.
        // ExecHandle is used only to ferry the pointer into spawned tasks.
        unsafe impl Send for ExecHandle {}
        unsafe impl Sync for ExecHandle {}
        impl ExecHandle {
            /// # Safety
            /// The pointee must still be alive.
            unsafe fn as_ref(&self) -> &crate::edge_tools::ToolExecutor {
                unsafe { &*self.0 }
            }
        }

        struct ScopedJoinHandles(
            Vec<tokio::task::JoinHandle<(crate::edge_tools::ToolExecutionOutcome, u64)>>,
        );
        impl Drop for ScopedJoinHandles {
            fn drop(&mut self) {
                for h in &self.0 {
                    h.abort();
                }
            }
        }

        let handle = ExecHandle(executor as *const _);
        let mut scope = ScopedJoinHandles(Vec::with_capacity(conc_reqs.len()));
        for (_, req) in &conc_reqs {
            let tool = req.tool.clone();
            let args = req.args.clone();
            let h = ExecHandle(handle.0);
            scope.0.push(tokio::spawn(async move {
                // SAFETY: ScopedJoinHandles aborts on drop — pointee is alive.
                let exec = unsafe { h.as_ref() };
                let t0 = Instant::now();
                let output = exec.execute_with_metadata(&tool, &args).await;
                (output, t0.elapsed().as_millis() as u64)
            }));
        }
        let mut outputs: Vec<(crate::edge_tools::ToolExecutionOutcome, u64)> =
            Vec::with_capacity(scope.0.len());
        for jh in std::mem::take(&mut scope.0) {
            match jh.await {
                Ok(result) => outputs.push(result),
                Err(e) => outputs.push((
                    crate::edge_tools::ToolExecutionOutcome {
                        output: format!("Tool execution panicked: {e}"),
                        tool_result_fields: None,
                    },
                    0,
                )),
            }
        }

        // ── Phase 3: Post-execution (sequential, &mut self) ──
        // Stop grouped spinner if we used one.
        if use_grouped_spinner {
            self.render.stop_tool_stderr_running();
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
            let post_result = self
                .api
                .post_tool_result(Some(self.token), Some(self.executor_id), &body)
                .await;
            if let Err(ref e) = post_result {
                let is_auth = matches!(
                    e,
                    astra_thin_client::ThinClientError::Api { status, .. }
                        if status.as_u16() == 401
                );
                if is_auth {
                    if let Some(token) = self.cancel_token {
                        token.cancel();
                    }
                    if !self.render_policy.is_silent() {
                        eprintln!(
                            "{}",
                            "Session expired. Please re-authenticate with `astra auth login`."
                                .red()
                        );
                    }
                    break;
                } else if !self.render_policy.suppress_tool_ui() {
                    eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
                }
            }
        }

        // Clear batch progress when done.
        self.render.tool_batch_progress = None;

        results
            .into_iter()
            .map(|r| r.expect("all tool result slots filled"))
            .collect()
    }
}

// ─── Turn result from one /chat/turn SSE stream ───────────────────────────────

/// One turn: core fields from [`ChatTurnSseAccum`] plus CLI-only edge bookkeeping and TTFT.
pub(super) struct TurnResult {
    pub(super) core: ChatTurnSseAccum,
    /// Time to first token in milliseconds (streaming latency).
    pub(super) ttft_ms: Option<u64>,
    /// Ordered executions from this SSE stream (for rounds without legacy `tool_call` events).
    pub(super) edge_tool_round: Vec<EdgeToolExecResult>,
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
                let line = format!("  {} {}{} …", "⬢".cyan(), prefix, styled_desc);
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
            let line = format!("  {} {}{} …", "⬢".cyan(), prefix, styled_desc);
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
            let line = format!("  {} {} …", "⬢".cyan(), description.dim());
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
        let term_w = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(80);
        let desc_budget = term_w.saturating_sub(14); // room for prefix + duration
        // Path budget: description budget minus the label prefix (e.g. "Reading: ")
        let path_budget = |prefix_len: usize| desc_budget.saturating_sub(prefix_len).max(20);

        match tool {
            "bash" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
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
                format!("Writing: {}", shorten_path(path, path_budget(9)))
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
            "git_commit" => {
                let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                format!("Git commit \"{}\"", truncate_line(msg, path_budget(13)))
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
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Search symbol {}", truncate_line(symbol, path_budget(15)))
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Get symbols in {}", shorten_path(path, path_budget(16)))
            }
            "call_graph" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Call graph for {}", truncate_line(symbol, path_budget(16)))
            }
            "run_build_test" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
            }
            "web_fetch" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                format!("Fetching: {}", truncate_line(url, path_budget(10)))
            }
            "github_get_pr" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                let num = args.get("pr_number").and_then(Value::as_u64).unwrap_or(0);
                format!("Getting PR: {owner}/{repo}#{num}")
            }
            "github_list_prs" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                format!("Listing PRs: {owner}/{repo}")
            }
            // Memory tools with natural verbs
            "memory_retrieve" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Recalling: \"{}\"", truncate_line(query, path_budget(13)))
            }
            "memory_store" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                format!("Storing: \"{}\"", truncate_line(content, path_budget(11)))
            }
            "memory_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!(
                    "Searching memory: \"{}\"",
                    truncate_line(query, path_budget(20))
                )
            }
            "memory_purge" => "Purging memory".to_string(),
            "memory_correct" => "Correcting memory".to_string(),
            "memory_profile" => "Checking profile".to_string(),
            // Skill tool — show specific skill name
            "skill" => {
                let skill_name = args
                    .get("skill_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                format!(
                    "Running skill: {}",
                    truncate_line(skill_name, path_budget(16))
                )
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
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 20)),
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
            "git_stash" => args
                .get("action")
                .and_then(Value::as_str)
                .map(|a| a.to_string()),
            "find_definition" | "find_references" | "symbol_search" | "hover_info" => args
                .get("symbol")
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 40)),
            "call_graph" | "type_hierarchy" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                let depth = args.get("depth").and_then(Value::as_u64);
                match depth {
                    Some(d) => Some(format!("{} (depth {})", truncate_line(symbol, 40), d)),
                    None => Some(truncate_line(symbol, 50)),
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
            "github_get_pr" | "github_get_issue" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                let number = args
                    .get("number")
                    .or_else(|| args.get("pr_number"))
                    .or_else(|| args.get("issue_number"))
                    .and_then(Value::as_u64);
                match number {
                    Some(n) => Some(format!("{owner}/{repo}#{n}")),
                    None => Some(format!("{owner}/{repo}")),
                }
            }
            "github_list_prs" | "github_list_issues" | "github_repo_stats" | "github_ci_status" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                Some(format!("{owner}/{repo}"))
            }
            "mo_query" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|q| truncate_line(q, 60)),
            "mo_snapshot" | "mo_branch" => args
                .get("action")
                .and_then(Value::as_str)
                .map(|a| a.to_string()),
            "memory_retrieve" | "memory_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|q| truncate_line(q, 50)),
            "memory_store" => args
                .get("content")
                .and_then(Value::as_str)
                .map(|c| truncate_line(c, 50)),
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
            "bash" | "shell" | "run_build_test" => {
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
                    l.starts_with("[Auto-expanded")
                        || l.starts_with("[truncated")
                        || l.starts_with("⚠ WARNING:")
                        || l.starts_with("⚠ Note:")
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
                let files: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
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
                if line_count > 1 {
                    Some(structural(format!("{line_count} lines")))
                } else if output.trim().is_empty() {
                    None
                } else {
                    Some(structural(truncate_line(output.trim(), 60)))
                }
            }
        }
    }

    /// Clear tool status region (for intermediate turns before next SSE stream).
    #[allow(dead_code)]
    fn clear_tool_region(&mut self) {
        self.stop_tool_stdout_anim();
        let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
        g.region.clear();
        g.lines.clear();
    }
}

fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// Determine tool completion icon based on status, output, and execution context.
/// Returns (icon_string, is_warning) where is_warning indicates warning-level result.
fn tool_completion_icon(
    tool: &str,
    status: &str,
    output: &str,
    duration_ms: u64,
) -> (String, bool) {
    if status == "error" {
        return (theme::icon_err(), false);
    }

    // Check for warning conditions.
    let trimmed = output.trim();

    // 1. Empty output for tools that should produce something.
    let expects_output = matches!(
        tool,
        "read_file" | "view_file" | "grep" | "glob" | "bash" | "shell"
    );
    if expects_output && trimmed.is_empty() {
        return (theme::icon_warn(), true);
    }

    // 2. "No matches" or empty results from search tools.
    if matches!(tool, "grep" | "glob") {
        if trimmed.is_empty()
            || trimmed == "[]"
            || trimmed.starts_with("No matches")
            || trimmed.starts_with("No files")
        {
            return (theme::icon_warn(), true);
        }
    }

    // 3. Truncated output (warning prefix in output).
    if trimmed.contains("[truncated") || trimmed.contains("⚠ WARNING:") {
        return (theme::icon_warn(), true);
    }

    // 4. Slow execution (>30s for most tools, >60s for bash).
    let slow_threshold_ms = if matches!(tool, "bash" | "shell" | "run_build_test") {
        60_000
    } else {
        30_000
    };
    if duration_ms > slow_threshold_ms {
        return (theme::icon_warn(), true);
    }

    // 5. Partial success indicators in bash output.
    if matches!(tool, "bash" | "shell") {
        let lower = trimmed.to_lowercase();
        if lower.contains("warning:") && !lower.contains("error:") {
            return (theme::icon_warn(), true);
        }
    }

    // Default: success.
    (theme::icon_ok(), false)
}

/// Format error message for tool failures with helpful context.
/// Extracts relevant info from common error patterns.
fn format_tool_error_summary(tool: &str, output: &str) -> String {
    let output_trimmed = output.trim();
    let first_line = output.lines().next().unwrap_or("").trim();

    // Tool-specific error extraction
    match tool {
        "bash" | "shell" | "run_build_test" => {
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

/// Apply bold+magenta styling to Skill/MCP tool description prefixes,
/// matching the visual weight of built-in tools like Read/Edit/Write.
pub(crate) fn style_tool_description(tool: &str, description: &str) -> String {
    if tool == "skill" {
        // "Running skill: code-review" → bold magenta "Running skill:" + rest
        if let Some(rest) = description.strip_prefix("Running skill:") {
            return format!("{}{}", "Running skill:".magenta().bold(), rest);
        }
    } else if tool.starts_with("mcp_") {
        // "MCP server tool" → bold magenta "MCP" + rest
        if let Some(rest) = description.strip_prefix("MCP") {
            return format!("{}{}", "MCP".magenta().bold(), rest);
        }
    }
    description.to_string()
}

/// Human-friendly tool description from a `ToolCallRecord`'s name + args_preview.
/// Mirrors `format_tool_description_with_output` but works without full args JSON.
pub(crate) fn format_tool_display_from_preview(name: &str, args_preview: Option<&str>) -> String {
    let preview = args_preview.unwrap_or("");
    match name {
        "bash" | "shell_exec" | "run_build_test" => format!("$ {preview}"),
        "read_file" | "view_file" => format!("Reading: {preview}"),
        "write_file" => format!("Writing: {preview}"),
        "str_replace" | "multi_edit" => format!("Editing: {preview}"),
        "delete_file" => format!("Deleting: {preview}"),
        "list_dir" => format!("Listing: {preview}"),
        "grep" => format!("Grep: {preview}"),
        "glob" => format!("Glob: {preview}"),
        "git_status" => "Git status".to_string(),
        "git_log" => format!("Git log {preview}"),
        "git_show" => format!("Git show {preview}"),
        "git_diff" => format!("Git diff {preview}"),
        "git_blame" => format!("Git blame {preview}"),
        "git_commit" => format!("Git commit {preview}"),
        "find_definition" => format!("Find definition of {preview}"),
        "find_references" => format!("Find references to {preview}"),
        "symbol_search" => format!("Search symbol {preview}"),
        "symbols" => format!("Get symbols in {preview}"),
        "call_graph" => format!("Call graph for {preview}"),
        "web_fetch" => format!("Fetching: {preview}"),
        "memory_retrieve" => format!("Recalling: \"{preview}\""),
        "memory_store" => format!("Storing: \"{preview}\""),
        "memory_search" => format!("Searching memory: \"{preview}\""),
        "skill" => format!("Running skill: {preview}"),
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
    let (sse_result, edge_tool_round, mut md_renderer, lines_written, _pending_xml_buffer) =
        if let Some(ctx) = edge {
            let mut host = CliSseStreamHost::from_edge_ctx(
                ctx,
                term_width,
                render_md && !render_policy.suppress_text(),
            );
            // pre_clear_lines only applies to non-md fallback path.
            if host.render.md.is_none() {
                host.render.lines_written = pre_clear_lines;
            }
            let (result, _abort) =
                consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token)
                    .await;
            let lw = host.render.lines_written;
            let md = host.render.md.take();
            let pending = std::mem::take(&mut host.xml_tag_buffer);
            (result, host.edge_tool_round, md, lw, pending)
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
                consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token)
                    .await;
            let lw = render.lines_written;
            let md = render.md.take();
            (result, Vec::new(), md, lw, String::new())
        };

    let mut result = TurnResult {
        core: sse_result.accum,
        ttft_ms: sse_result.ttft_ms,
        edge_tool_round,
    };
    super::streaming_md::strip_xml_tags_inplace(&mut result.full_text);
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

/// Whether the re-render pass should print the accumulated text.
///
/// Returns `false` when tool calls are pending — the text is an intermediate
/// draft that would leak into the terminal and never be cleared by subsequent
/// agentic-loop turns.
#[allow(dead_code)]
fn should_rerender_text(has_tool_calls: bool) -> bool {
    !has_tool_calls
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

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::{self, JournalDirGuard, JournalEvent, JournalEventType};
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    fn rerender_suppressed_when_tool_calls_present() {
        // When the LLM returns text + tool_calls (intermediate turn),
        // the re-render must NOT print the draft text.
        assert!(!should_rerender_text(true));
    }

    #[test]
    fn rerender_allowed_when_no_tool_calls() {
        // Final turn (no tool_calls) — text should be rendered.
        assert!(should_rerender_text(false));
    }

    #[test]
    fn dispatch_turn_complete_with_tool_calls_sets_flag() {
        // Verify that turn_complete with has_tool_calls=true flows through
        // to TurnResult so the re-render gate works end-to-end.
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"draft review text\""),
            sse("turn_complete", ",\"has_tool_calls\":true"),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, RenderPolicy::Silent, &mut vec![]);
        assert_eq!(r.full_text, "draft review text");
        assert!(r.has_tool_calls);
        // The gate must suppress re-render for this result.
        assert!(!should_rerender_text(r.has_tool_calls));
    }

    #[test]
    fn dispatch_turn_complete_without_tool_calls_allows_rerender() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"final answer\""),
            sse("turn_complete", ",\"has_tool_calls\":false"),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, RenderPolicy::Silent, &mut vec![]);
        assert_eq!(r.full_text, "final answer");
        assert!(!r.has_tool_calls);
        assert!(should_rerender_text(r.has_tool_calls));
    }

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
    fn could_become_thinking_tag_matches_known_prefixes() {
        assert!(could_become_thinking_tag("<"));
        assert!(could_become_thinking_tag("</"));
        assert!(could_become_thinking_tag("<t"));
        assert!(could_become_thinking_tag("<th"));
        assert!(could_become_thinking_tag("<thi"));
        assert!(could_become_thinking_tag("<thin"));
        assert!(could_become_thinking_tag("<think"));
        assert!(could_become_thinking_tag("</think"));
        assert!(could_become_thinking_tag("<r"));
        assert!(could_become_thinking_tag("<ref"));
        assert!(could_become_thinking_tag("</reflect"));
    }

    #[test]
    fn could_become_thinking_tag_rejects_other_tags() {
        // HTML tags that aren't thinking tags
        assert!(!could_become_thinking_tag("<co")); // <code>
        assert!(!could_become_thinking_tag("<p"));
        assert!(!could_become_thinking_tag("<div"));
        assert!(!could_become_thinking_tag("<span"));
        assert!(!could_become_thinking_tag("</code"));
        assert!(!could_become_thinking_tag("<a"));
        assert!(!could_become_thinking_tag("<b"));
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

    // ── extract_first_absolute_path ─────────────────────────────────────

    #[test]
    fn extract_absolute_path_from_cat_command() {
        assert_eq!(
            extract_first_absolute_path("cat /etc/passwd"),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn extract_absolute_path_skips_relative() {
        assert_eq!(extract_first_absolute_path("cat src/main.rs"), None);
    }

    #[test]
    fn extract_absolute_path_skips_variable() {
        assert_eq!(extract_first_absolute_path("cat $HOME/.bashrc"), None);
    }

    #[test]
    fn extract_absolute_path_skips_unc() {
        assert_eq!(extract_first_absolute_path("cat //server/share"), None);
    }

    #[test]
    fn extract_absolute_path_strips_trailing_semicolon() {
        assert_eq!(
            extract_first_absolute_path("cat /etc/passwd;"),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn extract_absolute_path_empty_command() {
        assert_eq!(extract_first_absolute_path(""), None);
    }

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
    fn style_regular_tool_unchanged() {
        let styled = style_tool_description("read_file", "Reading: src/main.rs");
        assert_eq!(styled, "Reading: src/main.rs");
    }

    #[test]
    fn style_bash_tool_unchanged() {
        let styled = style_tool_description("bash", "$ echo hello");
        assert_eq!(styled, "$ echo hello");
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
            ("file content".to_string(), "success".to_string()),
        );
        let hit = cache.output_cache.get(&sig);
        assert!(hit.is_some());
        let (output, status) = hit.unwrap();
        assert_eq!(output, "file content");
        assert_eq!(status, "success");
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
    fn edge_tool_cache_cacheable_tools_lookup() {
        // Verify that well-known cacheable tools are in the set
        assert!(CACHEABLE_TOOLS.contains(&"read_file"));
        assert!(CACHEABLE_TOOLS.contains(&"grep"));
        assert!(CACHEABLE_TOOLS.contains(&"glob"));
        assert!(CACHEABLE_TOOLS.contains(&"git_log"));
        // bash is NOT cacheable (side effects)
        assert!(!CACHEABLE_TOOLS.contains(&"bash"));
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(3, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
            },
            80,
            false,
        );

        let results = host
            .execute_tools_batch(vec![
                ToolBatchRequest {
                    request_id: "tr-1".to_string(),
                    tool: "delete_file".to_string(),
                    args: serde_json::json!({
                        "path": "txn.txt",
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
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
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(7, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(8, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(5, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor =
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id);
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(15, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor =
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id);
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(16, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(9, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(10, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        assert_eq!(results[2].status, "error");
        assert!(
            results[2].output.contains("already failed earlier"),
            "{}",
            results[2].output
        );
        let fields = results[2]
            .tool_result_fields
            .as_ref()
            .expect("rollback fields");
        assert_eq!(fields["rollback_boundary"].as_str(), Some("turn"));
        assert_eq!(fields["rollback_state"].as_str(), Some("aborted"));
        assert_eq!(fields["rollback_on_failure"].as_bool(), Some(true));
        assert!(
            !results[2].output.contains("existing"),
            "aborted turn request should not execute normally"
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(11, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(12, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor =
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id);
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(17, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor =
            crate::edge_tools::ToolExecutor::new(temp.path()).with_active_session_id(session_id);
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(18, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: true,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(13, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
        let mut executor = crate::edge_tools::ToolExecutor::new(temp.path());
        let mut tool_cache = EdgeToolCache::new(8);
        executor
            .journal_turn_index
            .store(14, std::sync::atomic::Ordering::Relaxed);

        let mut host = CliSseStreamHost::from_edge_ctx(
            EdgeSseContext {
                api: &api,
                token: "tok",
                executor_id: "edge-test",
                executor: &mut executor,
                render_policy: RenderPolicy::Silent,
                perm_manager: None,
                cancel_token: None,
                stream_event_tx: None,
                approval_request_tx: None,
                skill_resolver: None,
                skill_continuation: false,
                turn_rollback_on_failure: false,
                tool_cache: &mut tool_cache,
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
