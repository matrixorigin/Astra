//! Unified history-cell model for the refactored TUI.
//!
//! See `docs/design/tui-refactor.md` for the architectural rationale.
//! In short: every on-screen cell in the chat view implements
//! [`HistoryCell`]. A single owning structure (`ChatWidget.history:
//! Vec<Arc<dyn HistoryCell>>`) is the *one* source of truth — there is
//! no parallel transcript buffer, no ANSI-blob store. Persistence
//! (`TurnEvent` → `~/.astra/transcripts/<sid>.jsonl`) is an explicit
//! per-cell operation, not a side-effect of rendering.
//!
//! ## Lifecycle
//!
//! A cell lives in one of two phases:
//!
//! - **Live** — actively receiving input (streaming tokens, thinking
//!   chunks, tool output). `is_live()` returns `true`. Held as
//!   `Option<Box<dyn HistoryCell>>` in the widget's `active_cell`
//!   slot. Never persisted yet.
//! - **Committed** — `finalize()` has been called. Immutable
//!   thereafter. Moved into `history: Vec<Arc<dyn HistoryCell>>` and
//!   appended to the on-disk JSONL via `to_persist()`.
//!
//! A cell's **view** (`display_lines`) must be pure and cheap: it
//! runs on every frame the cell is visible. Any expensive rendering
//! (markdown parsing, syntax highlighting) should happen once, on
//! mutation — not inside `display_lines`.

pub(crate) mod approval;
pub(crate) mod assistant;
pub(crate) mod reasoning;
pub(crate) mod system;
pub(crate) mod task;
pub(crate) mod tool;
pub(crate) mod turn_summary;
pub(crate) mod user;

use std::any::Any;
use std::fmt::Debug;

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::turn_event::TurnEvent;

/// The single trait every chat-view row implements. Deliberately
/// small; specialised behaviour belongs to concrete cell types, not
/// to trait defaults.
///
/// The trait is object-safe so the widget can store
/// `Vec<Arc<dyn HistoryCell>>`.
pub(crate) trait HistoryCell: Debug + Send + Sync + Any {
    // ── Required ─────────────────────────────────────────────────

    /// Render this cell at the given terminal width. The output is
    /// one or more `Line`s and is appended directly to scrollback.
    ///
    /// Implementations must be pure + cheap. Cache heavy work during
    /// mutation (e.g. `push_delta`), not on each call.
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    /// Downcast helpers. Needed because the widget owns trait
    /// objects but event handlers sometimes need the concrete cell
    /// (e.g. to push a streaming chunk onto a live AssistantCell).
    fn as_any_ref(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    // ── Optional ─────────────────────────────────────────────────

    /// True while the cell is being mutated. `false` after
    /// [`finalize`] runs. A cell that never streams (e.g. a
    /// `UserCell` built from a single submitted string) stays
    /// `false` from construction.
    fn is_live(&self) -> bool {
        false
    }

    /// Freeze the cell. Called exactly once at the transition from
    /// live → committed. Safe to implement as a no-op for cells
    /// that have no transient state.
    fn finalize(&mut self) {}

    /// Process-relative seconds (same basis as
    /// `tui::shimmer::elapsed_since_start`) at the moment
    /// `finalize()` ran. `None` while live or for cells that were
    /// never live. Used by the active-slot gradient gutter to lock
    /// its phase on freeze instead of snapping to `t = 0`.
    ///
    /// Only the cell types that can occupy the *active slot* —
    /// today: `AssistantCell`, `ReasoningCell`, `ToolCell` — need to
    /// override this. Cells that never live in the active slot
    /// (system, user, approval, turn_summary) can leave the default
    /// `None`: they don't render through `LiveFramedCell` and the
    /// gutter never queries them.
    fn frozen_phase(&self) -> Option<f32> {
        None
    }

    /// Turn this cell into a durable persistence record. Returning
    /// `None` marks the cell as ephemeral — it renders but is not
    /// written to the transcript JSONL (e.g. an in-flight status
    /// line that would be meaningless on resume).
    fn to_persist(&self) -> Option<TurnEvent> {
        None
    }

    /// Called on terminal resize. Default: no cache, `display_lines`
    /// handles width internally. Cells that memoise wrapped lines
    /// must invalidate that cache here.
    fn on_resize(&mut self, _width: u16) {}

    /// Total vertical rows this cell occupies at `width`. Default
    /// uses ratatui's wrap calculator. Override for cells whose
    /// layout is already width-aware and needs no re-wrapping.
    fn desired_height(&self, width: u16) -> u16 {
        let lines = self.display_lines(width);
        Paragraph::new(ratatui::text::Text::from(lines))
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }
}

/// How many blank rows should follow this cell in the transcript.
///
/// The transcript uses tighter spacing for compact system pairs
/// (`> /cmd` + `Result · ...`) and slightly roomier spacing after
/// primary content blocks like tool results and assistant replies.
pub(crate) fn trailing_blank_rows(cell: &dyn HistoryCell) -> usize {
    if cell.as_any_ref().is::<user::UserCell>() {
        // User cells already carry their own top/bottom breathing
        // room inside the tinted panel; adding another transcript
        // separator after them makes the scrollback feel double-spaced.
        return 0;
    }

    if cell.as_any_ref().is::<system::SystemCell>() {
        return 1;
    }

    if cell.as_any_ref().is::<assistant::AssistantCell>()
        || cell.as_any_ref().is::<reasoning::ReasoningCell>()
        || cell.as_any_ref().is::<tool::ToolCell>()
        || cell.as_any_ref().is::<task::TaskCell>()
        || cell.as_any_ref().is::<approval::ApprovalCell>()
        || cell.as_any_ref().is::<turn_summary::TurnSummaryCell>()
    {
        return 1;
    }

    1
}

/// Separator rows after `cell`, optionally taking the following cell
/// into account for layout pairings. Most cells just use the generic
/// trailing spacing above. User cards are special: they carry their
/// own internal top/bottom breathing room, but consecutive user cards
/// still need one plain separator row so they don't visually merge
/// into one large tinted slab.
pub(crate) fn separator_rows_after(
    cell: &dyn HistoryCell,
    next: Option<&dyn HistoryCell>,
) -> usize {
    if cell.as_any_ref().is::<user::UserCell>()
        && next.is_some_and(|next| next.as_any_ref().is::<user::UserCell>())
    {
        return 1;
    }
    trailing_blank_rows(cell)
}

/// Tracks the moment a live cell freezes so the gradient gutter can
/// pin its phase. Centralised here so all `HistoryCell` impls share
/// one stamping discipline:
///   * `stamp_now()` — first-write-wins stamp at finalize / complete.
///   * `revived()` — launch-independent sentinel for cells rebuilt
///     from persistence. Note: revived cells are *settled*, not
///     active — they render through `display_lines` directly with a
///     static `█` marker, not the animated gradient. The stamp
///     exists so that if a revived cell is ever (incorrectly) routed
///     through the active slot it still produces a deterministic,
///     non-flickering hue.
///   * `phase()` — feeds `frozen_phase()` via `shimmer::time_at`.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FreezeStamp(Option<std::time::Instant>);

impl FreezeStamp {
    /// First-write-wins. Subsequent calls are no-ops so re-entrant
    /// finalize/complete paths don't push the pinned phase forward.
    pub(crate) fn stamp_now(&mut self) {
        if self.0.is_none() {
            self.0 = Some(std::time::Instant::now());
        }
    }

    /// Stamp used by `from_persist` constructors. Pins all revived
    /// cells to the process time origin (= phase 0) so they share
    /// a deterministic, launch-independent gutter hue.
    pub(crate) fn revived() -> Self {
        Self(Some(crate::tui::shimmer::process_start()))
    }

    /// Process-relative phase in seconds, or `None` while live.
    pub(crate) fn phase(self) -> Option<f32> {
        self.0.map(crate::tui::shimmer::time_at)
    }

    #[cfg(test)]
    pub(crate) fn is_set(self) -> bool {
        self.0.is_some()
    }
}

/// Width-aware string truncation with Unicode display-width
/// accounting. Shared across history-cell renderers that need to
/// fit content into a fixed column budget.
pub(super) fn truncate_by_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    let mut width = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw + 1 > max_width {
            break;
        }
        width += cw;
        end = i + c.len_utf8();
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    //! Unit tests that pin trait defaults. Concrete cell tests live
    //! alongside the cell implementations in Phase 2.

    use super::*;
    use ratatui::text::Span;

    /// A minimal stub used to verify trait defaults independently of
    /// any real cell. Keeps this module's tests self-contained while
    /// the cell zoo is still being rebuilt.
    #[derive(Debug)]
    struct Stub {
        lines: Vec<Line<'static>>,
        live: bool,
        finalize_calls: u32,
        resize_calls: u32,
    }

    impl Stub {
        fn new(text: &str) -> Self {
            Self {
                lines: vec![Line::from(Span::raw(text.to_string()))],
                live: false,
                finalize_calls: 0,
                resize_calls: 0,
            }
        }
    }

    impl HistoryCell for Stub {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
        fn is_live(&self) -> bool {
            self.live
        }
        fn finalize(&mut self) {
            self.finalize_calls += 1;
            self.live = false;
        }
        fn on_resize(&mut self, _width: u16) {
            self.resize_calls += 1;
        }
    }

    #[test]
    fn default_to_persist_is_none() {
        // Every cell that wants to persist must opt in explicitly —
        // the default is ephemeral so accidentally persisting a
        // half-finished widget is impossible.
        let s = Stub::new("hello");
        assert!(s.to_persist().is_none());
    }

    #[test]
    fn default_is_live_is_false() {
        let s = Stub::new("hello");
        assert!(!s.is_live());
    }

    #[test]
    fn desired_height_reflects_wrapped_lines() {
        let s = Stub {
            lines: vec![
                Line::from(Span::raw("one")),
                Line::from(Span::raw("two")),
                Line::from(Span::raw("three")),
            ],
            live: false,
            finalize_calls: 0,
            resize_calls: 0,
        };
        // Width is wide enough that no visual wrapping happens → 3
        // logical lines, 3 rows.
        assert_eq!(s.desired_height(80), 3);
    }

    #[test]
    fn trait_object_roundtrip_preserves_display() {
        // Proves the trait is object-safe and that `dyn HistoryCell`
        // delegates correctly. If this stops compiling, we've broken
        // the Vec<Arc<dyn HistoryCell>> contract.
        let s: Box<dyn HistoryCell> = Box::new(Stub::new("boxed"));
        let out: String = s
            .display_lines(80)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|sp| sp.content.to_string())
            .collect();
        assert_eq!(out, "boxed");
    }

    #[test]
    fn finalize_and_resize_are_observable() {
        let mut s = Stub::new("x");
        s.live = true;
        s.finalize();
        assert_eq!(s.finalize_calls, 1);
        assert!(!s.live, "finalize flips is_live → false");
        s.on_resize(60);
        s.on_resize(40);
        assert_eq!(s.resize_calls, 2);
    }
}
