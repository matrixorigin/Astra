//! `ChatWidget` — the single event router for the refactored TUI.
//!
//! Owns everything related to the scrollback + active stream:
//! the committed history (`Vec<Arc<dyn HistoryCell>>`), the
//! `active_cell: Option<Box<dyn HistoryCell>>` slot, and the
//! session identity. Does NOT own the composer / bottom pane /
//! popup menus — those stay in `BottomPane` because they're a
//! separate concern (input vs. output).
//!
//! The event flow is:
//!
//! ```text
//! AppEvent ──▶ ChatWidget::handle_event ──▶ mutate history/active_cell
//!                                         ──▶ append TurnEvent to disk
//!                                         ──▶ (outer draws on next frame)
//! ```
//!
//! `handle_event` is deliberately one big `match` (§3.2 of the
//! design doc). A reducer abstraction was tried and failed — the
//! async HTTP stream + direct terminal IO don't map cleanly to pure
//! `State, Action -> State`. One readable match beats a reducer that
//! leaks `Effect`s everywhere.
//!
//! All non-trait callers still live in `tui/mod.rs` in Phase 3 —
//! this module just provides the target API. Wire-up comes in
//! step 3d.

mod bridge;
mod resume;
#[cfg(test)]
mod turn_driver;

pub(crate) use bridge::{TurnContext, translate};
pub(crate) use resume::load as load_resume;

use std::sync::Arc;

use super::history_cell::{
    HistoryCell, assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell,
    tool::ToolCell, turn_summary::TurnSummaryCell, user::UserCell,
};
use super::transcript_jsonl;
use super::turn_event::TurnEvent;

/// Events the ChatWidget knows how to route. Every variant maps
/// 1:1 onto a state mutation. Compared to the legacy
/// `TuiAppEvent`, this enum is **self-contained** — no borrowed
/// references, no lifetimes — so it's easy to buffer, replay in
/// tests, and cross thread boundaries.
#[derive(Debug, Clone)]
pub(crate) enum AppEvent {
    /// User pressed Enter in the composer. Opens a new turn.
    UserSubmit(String),

    /// Token streamed as part of the model's final reply body.
    AnswerDelta(String),

    /// Chunk of reasoning / thinking content. Separate from
    /// `AnswerDelta` so the cell types don't get muddled —
    /// ReasoningCell vs AssistantCell are different things.
    ReasoningDelta(String),

    /// Server/host tells us reasoning has ended. Cells collapse
    /// into their finalised form on this signal.
    ReasoningDone,

    /// Server announced a new tool invocation starting.
    ToolStarted { name: String, description: String },

    /// Tool finished. `status` mirrors the string we receive on
    /// the wire ("success" / anything-else = failure).
    ToolCompleted {
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
    },

    /// Mid-flight progress signal for the active ToolCell —
    /// `lines`/`bytes` are cumulative counters since the tool
    /// started. Used to render real "streaming · N lines · K KB"
    /// status on long-running cells; the cell falls back to an
    /// indeterminate animation when this event never arrives (non-
    /// streaming tools like `read_file` / `git_log`).
    ToolOutput {
        name: String,
        lines: u64,
        bytes: u64,
    },

    /// Turn ended cleanly; ChatWidget should emit a summary cell.
    TurnComplete(Box<TurnStats>),

    /// Turn ended with an error. Error text gets humanised by
    /// `SystemCell::error` before storage.
    TurnError(String),
}

/// Per-turn metrics the outer loop collects and hands to
/// `TurnComplete` for the summary band. Boxed in the event enum
/// so the enum stays small (clippy::large_enum_variant guard).
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnStats {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Of the `tokens_in` total, how many were served from the
    /// provider's prompt cache. Drives the `💾 N%` segment in the
    /// per-turn summary band. `None` when the provider didn't
    /// report cache stats this turn (e.g. first turn, no cache
    /// participation, DeepSeek with cache disabled).
    pub cache_read_tokens: Option<u64>,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
}

/// Single source of truth for the chat-view scrollback.
///
/// `history` holds **committed** cells (finalised, persistable,
/// immutable). `active_cell` holds the **live** cell currently
/// being written to. Invariant: at most one `Some(active_cell)`
/// at a time; a new cell of a different kind swaps the old one
/// out through `commit_active()` before taking the slot.
pub(crate) struct ChatWidget {
    session_id: String,
    history: Vec<Arc<dyn HistoryCell>>,
    active_cell: Option<Box<dyn HistoryCell>>,
    /// Index into `history` marking cells that have already been
    /// flushed to the terminal scrollback. `drain_new_committed`
    /// returns everything past this index and advances it.
    committed_watermark: usize,
    /// Index into `history` marking cells that have already been
    /// persisted to the JSONL transcript. Starts at 0; advanced by
    /// `persist_from_watermark`. Kept separate from the display
    /// watermark because their lifecycles diverge — a cell is
    /// committed to scrollback as soon as it finalises, but may be
    /// held back from disk if the server hasn't yet assigned a
    /// session id (turn 1 edge case). When `set_session_id` is
    /// eventually called, we drain this watermark to persist every
    /// cell accumulated in the meantime.
    persist_watermark: usize,
}

impl ChatWidget {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            history: Vec::new(),
            active_cell: None,
            committed_watermark: 0,
            persist_watermark: 0,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn history(&self) -> &[Arc<dyn HistoryCell>] {
        &self.history
    }

    pub fn active_cell(&self) -> Option<&dyn HistoryCell> {
        self.active_cell.as_deref()
    }

    /// Drain cells added since the last call. The outer loop uses
    /// this to know which cells to flush into the terminal
    /// scrollback since the previous frame. Invariant: the
    /// returned cells are in the same order they were committed.
    ///
    /// Keeping a "consumed" watermark rather than a queue avoids
    /// copying; callers consume by iterating the returned slice.
    pub fn drain_new_committed(&mut self) -> Vec<Arc<dyn HistoryCell>> {
        let out = self.history[self.committed_watermark..].to_vec();
        self.committed_watermark = self.history.len();
        out
    }

    /// Reset the commit watermark to the current history length.
    /// Used on resume so replayed cells don't get reflushed.
    pub fn mark_all_flushed(&mut self) {
        self.committed_watermark = self.history.len();
    }

    /// Find the last committed `UserCell` and return its text, along
    /// with the number of history entries from that cell onward
    /// (caller may want to also truncate its own model-visible
    /// history, e.g. `state.history`, by the matching turn count).
    ///
    /// Does NOT mutate the widget — this is a pure query. The display
    /// scrollback is already painted to the terminal and cannot be
    /// unwritten; the caller decides what state (if any) to
    /// invalidate. `Ctrl+R` uses this to seed the composer with the
    /// prior user prompt for re-editing.
    pub fn last_user_text(&self) -> Option<String> {
        use super::history_cell::user::UserCell;
        self.history
            .iter()
            .rev()
            .find_map(|c| c.as_any_ref().downcast_ref::<UserCell>())
            .map(|cell| cell.text().to_string())
    }

    /// Swap the backing session id.
    ///
    /// - If cells accumulated under an empty sid (turn-1 edge case:
    ///   server hadn't assigned one yet), they get flushed to the
    ///   new session's JSONL transcript on first assignment. This
    ///   is what lets resume replay show the user's very first
    ///   message instead of starting mid-conversation.
    /// - Cells already persisted under a non-empty sid stay in
    ///   their original transcript; only the new cells ride under
    ///   the new id.
    pub fn set_session_id(&mut self, sid: impl Into<String>) {
        self.session_id = sid.into();
        // Flush any cells that accumulated while sid was empty.
        self.persist_from_watermark();
    }

    /// Replay a previously-persisted turn stream into `history`.
    /// Used by the Phase 4 resume path. Cells land already
    /// finalised — no live state, no further mutation.
    ///
    /// Advances the persist watermark past the replayed cells so
    /// subsequent `commit_*` calls don't re-persist them to the
    /// JSONL (which would double every resumed line on every
    /// future write).
    pub fn replay(&mut self, events: Vec<TurnEvent>) {
        for ev in events {
            if let Some(cell) = cell_from_persist(ev) {
                self.history.push(cell.into());
            }
        }
        self.persist_watermark = self.history.len();
    }

    /// Commit a free-standing `SystemCell` — slash-command responses,
    /// info banners, inline errors, etc. Goes into `history` and the
    /// JSONL transcript the same way model-generated cells do, so
    /// resume replay surfaces them and the Ctrl+O overlay keeps them.
    ///
    /// Before this, slash-dispatch wrote system lines directly to the
    /// terminal via `queue_history_lines` — they showed in the live
    /// scrollback but never made it to disk, so a resumed session
    /// silently lost every `/model`, `/login`, `/permission` response
    /// as well as the `Session expired` / "token refreshed" banners.
    pub fn commit_system(&mut self, cell: SystemCell) {
        self.commit_active(); // finalise anything live first
        self.commit_cell(Box::new(cell));
    }

    /// Single choke-point for routing events into state mutation.
    /// Any `AppEvent` emitted by the outer loop MUST go through
    /// here — nothing else in the TUI reaches into `history` or
    /// `active_cell`.
    pub fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::UserSubmit(text) => self.on_user_submit(text),
            AppEvent::AnswerDelta(d) => self.on_answer_delta(&d),
            AppEvent::ReasoningDelta(d) => self.on_reasoning_delta(&d),
            AppEvent::ReasoningDone => self.on_reasoning_done(),
            AppEvent::ToolStarted { name, description } => self.on_tool_started(name, description),
            AppEvent::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
            } => self.on_tool_completed(
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
            ),
            AppEvent::ToolOutput { name, lines, bytes } => self.on_tool_output(&name, lines, bytes),
            AppEvent::TurnComplete(stats) => self.on_turn_complete(*stats),
            AppEvent::TurnError(msg) => self.on_turn_error(msg),
        }
    }

    // ── Event handlers ───────────────────────────────────────────

    fn on_user_submit(&mut self, text: String) {
        // A new user turn implicitly finalises any live cell — the
        // previous turn is over whether it committed itself cleanly
        // or not.
        self.commit_active();
        let cell = UserCell::new(text);
        self.commit_cell(Box::new(cell));
    }

    fn on_answer_delta(&mut self, delta: &str) {
        // Tokens can begin flowing while a `ReasoningCell` is
        // still live (some providers end reasoning implicitly by
        // starting text); in that case we finalise the reasoning
        // cell first, then build a fresh AssistantCell.
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ) {
            self.commit_active();
        }

        // Create the AssistantCell on first delta if needed.
        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant)
        ) {
            self.active_cell = Some(Box::new(AssistantCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantCell>()
        {
            ac.push_delta(delta);
        }
    }

    fn on_reasoning_delta(&mut self, delta: &str) {
        // Reasoning arriving while a tool is live shouldn't
        // happen in practice, but just in case: commit the tool
        // cell first so the reasoning gets its own scrollback row
        // instead of overwriting.
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ) {
            self.commit_active();
        }

        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ) {
            self.active_cell = Some(Box::new(ReasoningCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(rc) = cell.as_any_mut().downcast_mut::<ReasoningCell>()
        {
            rc.push_delta(delta);
        }
    }

    fn on_reasoning_done(&mut self) {
        // Only flips the reasoning cell's live flag; the cell
        // stays in `active_cell` because the model might still
        // emit the answer, and keeping it there avoids an extra
        // commit+rebuild round-trip if it does.
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(rc) = cell.as_any_mut().downcast_mut::<ReasoningCell>()
        {
            rc.finalize();
            // Reasoning is done — commit it so the answer can land
            // as its own cell. Keeps the scrollback readable as
            // discrete turns rather than one blob.
            self.commit_active();
        }
    }

    fn on_tool_started(&mut self, name: String, description: String) {
        self.commit_active();
        self.active_cell = Some(Box::new(ToolCell::new_running(name, description)));
    }

    /// Route a `ToolOutput` progress tick to the currently active
    /// ToolCell. The `name` arg is advisory — we only forward if the
    /// active cell is a running tool; cells from a completed prior
    /// tool are ignored rather than synthesised, since progress
    /// without a live cell has no visual home.
    fn on_tool_output(&mut self, name: &str, lines: u64, bytes: u64) {
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<ToolCell>()
            && tc.name == name
        {
            tc.set_progress(lines, bytes);
        }
    }

    fn on_tool_completed(
        &mut self,
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
    ) {
        // Update the in-flight tool cell if one exists; otherwise
        // synthesize a new completed cell (happens when the model
        // emits a ToolCompleted without a paired ToolStarted —
        // e.g. replayed from journal mid-turn).
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<ToolCell>()
        {
            tc.complete(&status, duration_ms, description, output_summary, output);
            self.commit_active();
            return;
        }

        let mut synth = ToolCell::new_running(name, description);
        synth.complete(&status, duration_ms, String::new(), output_summary, output);
        self.commit_cell(Box::new(synth));
    }

    fn on_turn_complete(&mut self, stats: TurnStats) {
        // Any live cell at turn-complete time gets committed
        // unconditionally (the model ended the turn, so any
        // dangling stream is done).
        self.commit_active();

        let summary = TurnSummaryCell {
            elapsed_ms: stats.elapsed_ms,
            ttft_ms: stats.ttft_ms,
            tokens_in: stats.tokens_in,
            tokens_out: stats.tokens_out,
            cache_read_tokens: stats.cache_read_tokens,
            tools: stats.tools,
            cumulative_tokens: stats.cumulative_tokens,
            cumulative_cost_usd: stats.cumulative_cost_usd,
            ts: None,
        };
        self.commit_cell(Box::new(summary));
    }

    fn on_turn_error(&mut self, msg: String) {
        self.commit_active();
        self.commit_cell(Box::new(SystemCell::error(msg)));
    }

    // ── Invariant-preserving mutators ────────────────────────────

    /// Take the currently-live cell, finalise it, append to
    /// history, and persist. No-op when `active_cell` is None.
    fn commit_active(&mut self) {
        let Some(mut cell) = self.active_cell.take() else {
            return;
        };
        cell.finalize();
        // Box → Arc: the scrollback index shares cells with
        // long-lived render paths (e.g. Ctrl+O overlay) without
        // forcing everyone onto `&dyn`.
        self.history.push(box_into_arc(cell));
        self.persist_from_watermark();
    }

    /// Append an already-finalised cell. Used for UserCell /
    /// synthesised ToolCell / TurnSummary etc. — things built
    /// whole rather than streamed.
    fn commit_cell(&mut self, cell: Box<dyn HistoryCell>) {
        self.history.push(box_into_arc(cell));
        self.persist_from_watermark();
    }

    /// Persist every cell between `persist_watermark` and
    /// `history.len()`. Best-effort: errors are logged by the
    /// underlying `transcript_jsonl` helper and the watermark is
    /// advanced regardless, because the TUI must keep running and
    /// retrying a flaky write every turn would re-attempt the same
    /// failure.
    ///
    /// When `session_id` is empty (turn-1 edge case: server hasn't
    /// assigned an id yet) the watermark is NOT advanced, so
    /// subsequent `set_session_id` can flush the accumulated cells.
    fn persist_from_watermark(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        while self.persist_watermark < self.history.len() {
            let cell = &self.history[self.persist_watermark];
            if let Some(ev) = cell.to_persist() {
                transcript_jsonl::append(&self.session_id, &ev);
            }
            self.persist_watermark += 1;
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    System,
    TurnSummary,
    Other,
}

fn cell_kind(c: &dyn HistoryCell) -> CellKind {
    let a = c.as_any_ref();
    if a.is::<UserCell>() {
        CellKind::User
    } else if a.is::<AssistantCell>() {
        CellKind::Assistant
    } else if a.is::<ReasoningCell>() {
        CellKind::Reasoning
    } else if a.is::<ToolCell>() {
        CellKind::Tool
    } else if a.is::<SystemCell>() {
        CellKind::System
    } else if a.is::<TurnSummaryCell>() {
        CellKind::TurnSummary
    } else {
        CellKind::Other
    }
}

/// Dispatch a persisted `TurnEvent` to the matching cell builder.
/// Unknown events land as `None` — caller drops them, preserving
/// the "skip, don't crash" contract.
fn cell_from_persist(ev: TurnEvent) -> Option<Box<dyn HistoryCell>> {
    match ev {
        TurnEvent::User { .. } => {
            UserCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Assistant { .. } => {
            AssistantCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Thinking { .. } => {
            ReasoningCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Tool { .. } => {
            ToolCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::System { .. } => {
            SystemCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::TurnSummary { .. } => {
            TurnSummaryCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
    }
}

/// `Box<dyn HistoryCell>` → `Arc<dyn HistoryCell>` without
/// re-boxing the payload. Safe because `Arc::from` consumes the
/// box.
fn box_into_arc(b: Box<dyn HistoryCell>) -> Arc<dyn HistoryCell> {
    Arc::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::history_cell::tool::ToolStatus;

    fn fresh() -> ChatWidget {
        // Empty sid → persistence becomes a no-op (see
        // `transcript_jsonl::append` / `persist` guard). Lets
        // tests run without touching $HOME.
        ChatWidget::new("")
    }

    // ── UserSubmit ───────────────────────────────────────────────

    #[test]
    fn user_submit_appends_usercell_and_clears_active() {
        let mut w = fresh();
        // Simulate a prior live assistant cell that never got
        // finalised (e.g. turn got interrupted).
        w.active_cell = Some(Box::new(AssistantCell::new_streaming()));
        w.handle_event(AppEvent::UserSubmit("hello".into()));
        assert!(
            w.active_cell.is_none(),
            "UserSubmit must finalise any dangling live cell"
        );
        assert_eq!(w.history.len(), 2, "committed assistant + user");
        // Final entry is the user cell with the exact text.
        let last = w.history.last().unwrap();
        let persisted = last.to_persist().expect("user cell persists");
        assert!(matches!(&persisted, TurnEvent::User { text, .. } if text == "hello"));
    }

    // ── AnswerDelta ──────────────────────────────────────────────

    #[test]
    fn answer_delta_creates_assistant_then_accumulates() {
        let mut w = fresh();
        w.handle_event(AppEvent::AnswerDelta("Hello ".into()));
        w.handle_event(AppEvent::AnswerDelta("world".into()));
        let cell = w
            .active_cell
            .as_ref()
            .expect("active_cell should be live")
            .as_any_ref()
            .downcast_ref::<AssistantCell>()
            .expect("should be AssistantCell");
        assert_eq!(cell.source(), "Hello world");
        assert!(cell.is_live());
    }

    #[test]
    fn answer_delta_finalises_live_reasoning_cell() {
        let mut w = fresh();
        // Begin a reasoning cell then jump straight to answer —
        // models that don't emit ReasoningDone rely on this
        // transition.
        w.handle_event(AppEvent::ReasoningDelta("thinking".into()));
        w.handle_event(AppEvent::AnswerDelta("answer".into()));
        // Reasoning must be committed before the assistant cell
        // takes over.
        assert_eq!(w.history.len(), 1);
        let reasoning = &w.history[0];
        let ev = reasoning.to_persist().unwrap();
        assert!(matches!(ev, TurnEvent::Thinking { .. }));
        // Active cell is now the Assistant one.
        assert!(
            matches!(
                w.active_cell.as_deref().map(cell_kind),
                Some(CellKind::Assistant)
            ),
            "answer should supplant reasoning"
        );
    }

    // ── Reasoning lifecycle ──────────────────────────────────────

    #[test]
    fn reasoning_done_commits_reasoning_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::ReasoningDelta("step 1".into()));
        w.handle_event(AppEvent::ReasoningDone);
        assert_eq!(w.history.len(), 1, "reasoning cell committed");
        assert!(w.active_cell.is_none(), "active cleared after done");
    }

    #[test]
    fn reasoning_done_without_reasoning_is_noop() {
        let mut w = fresh();
        w.handle_event(AppEvent::ReasoningDone);
        assert_eq!(w.history.len(), 0);
        assert!(w.active_cell.is_none());
    }

    // ── Tool lifecycle ───────────────────────────────────────────

    #[test]
    fn tool_started_then_completed_commits_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::ToolStarted {
            name: "bash".into(),
            description: "ls /tmp".into(),
        });
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ));
        w.handle_event(AppEvent::ToolCompleted {
            name: "bash".into(),
            description: String::new(),
            status: "success".into(),
            duration_ms: 42,
            output_summary: Some("3 entries".into()),
            output: None,
        });
        assert_eq!(w.history.len(), 1);
        assert!(w.active_cell.is_none());

        let cell = w.history[0]
            .as_any_ref()
            .downcast_ref::<ToolCell>()
            .unwrap();
        assert_eq!(cell.status, ToolStatus::Success);
        assert_eq!(cell.duration_ms, Some(42));
    }

    #[test]
    fn unpaired_tool_completed_synthesises_cell() {
        // A bare ToolCompleted (no preceding ToolStarted) still
        // yields a committed cell. Defensive: journals can
        // sometimes replay events out of order.
        let mut w = fresh();
        w.handle_event(AppEvent::ToolCompleted {
            name: "bash".into(),
            description: "echo hi".into(),
            status: "success".into(),
            duration_ms: 10,
            output_summary: None,
            output: None,
        });
        assert_eq!(w.history.len(), 1);
    }

    // ── Turn lifecycle ───────────────────────────────────────────

    #[test]
    fn turn_complete_emits_summary() {
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("hi".into()));
        w.handle_event(AppEvent::AnswerDelta("answer".into()));
        w.handle_event(AppEvent::TurnComplete(Box::new(TurnStats {
            elapsed_ms: Some(1_500),
            tokens_in: Some(50),
            tokens_out: Some(10),
            tools: 0,
            ..Default::default()
        })));
        assert!(w.active_cell.is_none());
        // Expect: user cell + assistant cell + summary cell.
        assert_eq!(w.history.len(), 3);
        assert!(
            w.history
                .last()
                .unwrap()
                .as_any_ref()
                .downcast_ref::<TurnSummaryCell>()
                .is_some()
        );
    }

    #[test]
    fn turn_error_commits_system_error_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("hi".into()));
        w.handle_event(AppEvent::TurnError("<error>rate limited</error>".into()));
        assert_eq!(w.history.len(), 2);
        let err = w
            .history
            .last()
            .unwrap()
            .as_any_ref()
            .downcast_ref::<SystemCell>()
            .expect("last cell should be SystemCell");
        // Humanisation strips the tag.
        assert_eq!(err.message(), "rate limited");
    }

    // ── Invariant: at most one live cell ─────────────────────────

    #[test]
    fn tool_started_mid_stream_commits_assistant_first() {
        let mut w = fresh();
        w.handle_event(AppEvent::AnswerDelta("first half ".into()));
        w.handle_event(AppEvent::ToolStarted {
            name: "bash".into(),
            description: "ls".into(),
        });
        // Assistant should have been committed before the tool
        // took the active slot. Two cells in history: partial
        // assistant + (nothing else yet).
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ));
        assert_eq!(w.history.len(), 1);
        let ev = w.history[0].to_persist().unwrap();
        assert!(matches!(ev, TurnEvent::Assistant { .. }));
    }

    // ── Watermark / flush tracking ──────────────────────────────

    #[test]
    fn drain_new_committed_returns_only_unflushed_cells() {
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("a".into()));
        w.handle_event(AppEvent::UserSubmit("b".into()));
        // First drain returns both new cells.
        let first = w.drain_new_committed();
        assert_eq!(first.len(), 2, "first drain covers all so far");

        // Second drain returns nothing new.
        let second = w.drain_new_committed();
        assert!(second.is_empty(), "no new cells since first drain");

        // After another commit, only the delta.
        w.handle_event(AppEvent::UserSubmit("c".into()));
        let third = w.drain_new_committed();
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn mark_all_flushed_suppresses_existing_cells() {
        // Used by resume: after loading history we don't want to
        // reflush it into the terminal, the caller paints it once
        // and advances the watermark.
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("existing".into()));
        w.mark_all_flushed();
        let out = w.drain_new_committed();
        assert!(out.is_empty(), "marked-flushed cells must not redraw");

        // New cells after the mark still surface.
        w.handle_event(AppEvent::UserSubmit("new".into()));
        let out = w.drain_new_committed();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn set_session_id_swaps_without_losing_history() {
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("before".into()));
        assert_eq!(w.history().len(), 1);
        w.set_session_id("new-sid");
        assert_eq!(w.session_id(), "new-sid");
        assert_eq!(w.history().len(), 1, "history survives sid swap");
    }

    // ── Persist watermark (turn-1 edge case) ────────────────────

    /// Run a test body with `$HOME` pointed at a fresh tempdir so
    /// real `~/.astra/transcripts/` is left alone.
    fn with_tmp_home<F: FnOnce()>(f: F) {
        use std::env;
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = env::var("HOME").ok();
        unsafe {
            env::set_var("HOME", tmp.path());
        }
        f();
        match prev {
            Some(v) => unsafe { env::set_var("HOME", v) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial_test::serial]
    fn set_session_id_flushes_cells_committed_under_empty_sid() {
        // Turn 1 edge case: cells commit before the server returns
        // a session id. `set_session_id` must retroactively flush
        // them to the new session's JSONL, so resume replay can
        // surface the user's very first message.
        with_tmp_home(|| {
            let mut w = ChatWidget::new(""); // empty sid — server pending
            w.handle_event(AppEvent::UserSubmit("hi".into()));
            w.handle_event(AppEvent::AnswerDelta("hello back".into()));
            w.handle_event(AppEvent::TurnComplete(Box::default()));

            // Before sid is set, nothing should be on disk yet.
            assert!(super::super::transcript_jsonl::load("late-sid").is_empty());

            // Server finally assigns an id → we flush retroactively.
            w.set_session_id("late-sid");
            let events = super::super::transcript_jsonl::load("late-sid");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::User { text, .. } if text == "hi")),
                "turn-1 user message must be persisted after sid arrives"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::Assistant { .. })),
                "turn-1 assistant reply must be persisted after sid arrives"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::TurnSummary { .. })),
                "turn-1 summary must be persisted after sid arrives"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn post_sid_cells_dont_double_persist_previous_cells() {
        // Sanity: after the initial flush, committing another turn
        // must append only that turn's cells — we must NOT re-write
        // the earlier turn's cells. This is what the persist
        // watermark guards against.
        with_tmp_home(|| {
            let mut w = ChatWidget::new("");
            w.handle_event(AppEvent::UserSubmit("first".into()));
            w.handle_event(AppEvent::TurnComplete(Box::default()));
            w.set_session_id("s");

            let count_after_first = super::super::transcript_jsonl::load("s").len();

            w.handle_event(AppEvent::UserSubmit("second".into()));
            w.handle_event(AppEvent::TurnComplete(Box::default()));
            let count_after_second = super::super::transcript_jsonl::load("s").len();

            assert!(
                count_after_second > count_after_first,
                "second turn must add cells: {count_after_first} → {count_after_second}"
            );
            // Each commit cycle (UserSubmit + TurnComplete) adds 2
            // cells: the user + the summary. Duplicate persistence
            // would give us 4 new rows instead of 2.
            assert_eq!(
                count_after_second - count_after_first,
                2,
                "second turn should append exactly 2 cells, not double-write earlier ones"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn replay_does_not_re_persist_resumed_cells() {
        // When resuming a session, cells land in `history` via
        // `replay()` — they already exist on disk. A subsequent
        // `commit_*` must not re-persist the replayed cells.
        with_tmp_home(|| {
            let sid = "s_replay";
            // Seed a one-turn session on disk.
            super::super::transcript_jsonl::append(
                sid,
                &TurnEvent::User {
                    ts: None,
                    text: "seed".into(),
                },
            );
            let before = super::super::transcript_jsonl::load(sid).len();
            assert_eq!(before, 1);

            let mut w = ChatWidget::new(sid);
            w.replay(super::super::transcript_jsonl::load(sid));
            // Commit a new cell — only this cell should land on disk.
            w.handle_event(AppEvent::UserSubmit("new".into()));
            let after = super::super::transcript_jsonl::load(sid).len();
            assert_eq!(
                after,
                before + 1,
                "only the new cell should persist; {before} → {after}"
            );
        });
    }

    // ── Last user text lookup (Ctrl+R edit-last) ────────────────

    #[test]
    fn last_user_text_walks_back_past_trailing_cells() {
        // History ends with non-User cells (assistant + summary);
        // lookup must still surface the most recent user message.
        let mut w = fresh();
        w.handle_event(AppEvent::UserSubmit("first".into()));
        w.handle_event(AppEvent::AnswerDelta("reply 1".into()));
        w.handle_event(AppEvent::TurnComplete(Box::default()));
        w.handle_event(AppEvent::UserSubmit("second".into()));
        w.handle_event(AppEvent::AnswerDelta("reply 2".into()));
        w.handle_event(AppEvent::TurnComplete(Box::default()));

        assert_eq!(w.last_user_text().as_deref(), Some("second"));
    }

    #[test]
    fn last_user_text_none_on_empty_history() {
        let w = fresh();
        assert!(w.last_user_text().is_none());
    }

    // ── Replay ──────────────────────────────────────────────────

    #[test]
    fn replay_reconstructs_history_in_order() {
        let mut w = fresh();
        let events = vec![
            TurnEvent::User {
                ts: None,
                text: "hi".into(),
            },
            TurnEvent::Assistant {
                ts: None,
                markdown: "hello".into(),
            },
            TurnEvent::TurnSummary {
                ts: None,
                elapsed_ms: Some(100),
                ttft_ms: None,
                tokens_in: Some(10),
                tokens_out: Some(5),
                cache_read_tokens: None,
                tools: 0,
                cumulative_tokens: Some(15),
                cumulative_cost_usd: None,
            },
        ];
        w.replay(events);
        assert_eq!(w.history.len(), 3);
        assert!(
            w.history[0].as_any_ref().is::<UserCell>(),
            "first should be User"
        );
        assert!(
            w.history[1].as_any_ref().is::<AssistantCell>(),
            "second should be Assistant"
        );
        assert!(
            w.history[2].as_any_ref().is::<TurnSummaryCell>(),
            "third should be TurnSummary"
        );
    }
}
