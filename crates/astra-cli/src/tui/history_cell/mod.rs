//! Unified history-cell model for the refactored TUI.
//!
//! See `docs/design/tui-refactor.md` for the architectural rationale.
//! In short: every on-screen cell in the chat view implements
//! [`HistoryCell`]. A single owning structure (`ChatWidget.history:
//! Vec<Arc<dyn HistoryCell>>`) is the source of truth for the live view —
//! there is no parallel transcript buffer or ANSI-blob store. The durable
//! transcript is the canonical session journal; compact [`TurnEvent`] values
//! are an in-memory adapter for rebuilding cells from that journal.
//!
//! ## Lifecycle
//!
//! A cell lives in one of two phases:
//!
//! - **Live** — actively receiving input (streaming tokens, thinking
//!   chunks, tool output). `is_live()` returns `true`. Held as
//!   `Option<Box<dyn HistoryCell>>` in the widget's `active_cell`
//!   slot. Never durably recorded by the view itself.
//! - **Committed** — `finalize()` has been called. Immutable
//!   thereafter. Moved into `history: Vec<Arc<dyn HistoryCell>>`. The turn
//!   pipeline independently records its canonical transcript event.
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

    /// Convert this cell to the compact restoration form used by the journal
    /// resume adapter and cell-level tests. Returning `None` marks a purely
    /// local UI row (for example an in-flight status line) that must never be
    /// reconstructed as conversation history.
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
    if cell
        .as_any_ref()
        .downcast_ref::<system::SystemCell>()
        .is_some_and(|cell| cell.level() == super::turn_event::SystemLevel::Action)
        && next.is_some_and(|next| next.as_any_ref().is::<system::SystemCell>())
    {
        return 0;
    }
    if cell.as_any_ref().is::<user::UserCell>()
        && next.is_some_and(|next| next.as_any_ref().is::<user::UserCell>())
    {
        return 1;
    }
    trailing_blank_rows(cell)
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
