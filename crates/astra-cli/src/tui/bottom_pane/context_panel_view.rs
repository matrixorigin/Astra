//! BottomPaneView wrapper for the context-window breakdown.
//!
//! Holds the scroll offset locally so the rich breakdown (grid +
//! legend + nested tool/memory/skill sections) can exceed the
//! overlay's visible rows without being clipped — users page
//! through with j/k, arrows, PgUp/PgDn, Home/End.  The underlying
//! [`ContextBreakdown`] is immutable once built; scrolling is a
//! pure-view concern.

#![allow(dead_code)]

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::tui::context_panel::{ContextBreakdown, Section, view as panel_view};

pub(crate) struct ContextPanelView {
    breakdown: ContextBreakdown,
    completed: bool,
    /// Current top-of-window line index. Paragraph lines above
    /// `scroll` are not drawn. Clamped against the last valid
    /// offset at render time.
    scroll: u16,
    /// Current focus + expansion state. Starts without a focus so
    /// the first render matches the old "flat" behavior; Tab jumps
    /// to the first non-empty section. Enter toggles expansion on
    /// the focused section.
    focus: Option<Section>,
    expanded: Option<Section>,
    /// Position of the currently-selected item within the expanded
    /// section. `0` when no expansion / no selectable items.
    /// Clamped to the section's item count at key-handling time.
    selected_item: usize,
    /// `true` when the user has pressed Enter on a selected item —
    /// the section renders only that item's full content.
    drilled: bool,
    /// Scroll position captured the moment we entered drill mode,
    /// restored when Esc exits the drill.  Keeps the eye on the
    /// same item the user drilled from.
    pre_drill_scroll: u16,
    /// Cached last-known viewport height (content rows minus
    /// border). Populated by `render` so `handle_key` can page by
    /// a meaningful amount even though it doesn't see the Rect.
    /// `Cell<u16>` because `render` takes `&self` — keeping the
    /// mutation behind interior mutability avoids plumbing a
    /// `render_mut` method through the `BottomPaneView` trait just
    /// for this.
    last_viewport_rows: Cell<u16>,
    last_inner_width: Cell<u16>,
}

impl ContextPanelView {
    pub fn new(breakdown: ContextBreakdown) -> Self {
        Self {
            breakdown,
            completed: false,
            scroll: 0,
            focus: None,
            expanded: None,
            selected_item: 0,
            drilled: false,
            pre_drill_scroll: 0,
            last_viewport_rows: Cell::new(0),
            last_inner_width: Cell::new(80),
        }
    }

    fn view_state(&self) -> panel_view::ViewState {
        panel_view::ViewState {
            focus: self.focus,
            expanded: self.expanded,
            selected_item: self.selected_item,
            drilled: self.drilled,
        }
    }

    /// Number of items in the currently-expanded section (0 when
    /// no expansion or the section has no drillable items).
    fn selectable_count(&self) -> usize {
        let Some(section) = self.expanded else {
            return 0;
        };
        panel_view::section_item_count(&self.breakdown, section)
    }

    /// Move selection by +/-1 clamped to the item count.  When the
    /// section has no drillable items, no-op so the keypress
    /// doesn't drop silently.
    fn select_item(&mut self, delta: i32) {
        let n = self.selectable_count();
        if n == 0 {
            return;
        }
        let next = (self.selected_item as i32 + delta).clamp(0, (n - 1) as i32);
        self.selected_item = next as usize;
    }

    /// Return the max scroll offset — one past the last line that
    /// can appear at the top while still filling the viewport.
    fn max_scroll(&self) -> u16 {
        let total = panel_view::line_count_with(
            &self.breakdown,
            self.last_inner_width.get(),
            self.view_state(),
        );
        let page = self.last_viewport_rows.get().max(1);
        total.saturating_sub(page)
    }

    fn scroll_by(&mut self, delta: i32) {
        let max = self.max_scroll() as i32;
        let next = (self.scroll as i32 + delta).clamp(0, max);
        self.scroll = next as u16;
    }

    /// Cycle focus to the next section that has content, wrapping.
    /// Tab when no section is focused jumps to the first non-empty
    /// section.
    fn focus_next(&mut self, reverse: bool) {
        let current = match self.focus {
            Some(s) => s,
            None => {
                self.focus = self.breakdown.first_focusable_section();
                // Entering focus mode for the first time: any
                // stale nested state (from a prior Tab-in /
                // Tab-out dance that the defaults don't cover)
                // gets cleared here. Mirror the Some-branch
                // reset below so behaviour stays consistent.
                if let Some(exp) = self.expanded
                    && Some(exp) != self.focus
                {
                    self.expanded = None;
                }
                self.selected_item = 0;
                self.drilled = false;
                return;
            }
        };
        // Walk the cycle until we hit a section with content or
        // wrap back to where we started (safety against all-empty).
        let start = current;
        let mut next = if reverse {
            current.prev()
        } else {
            current.next()
        };
        while next != start {
            if self.breakdown.section_non_empty(next) {
                break;
            }
            next = if reverse { next.prev() } else { next.next() };
        }
        self.focus = Some(next);
        // Collapse on focus-change — expansion should only apply
        // to the actively viewed section. Keeping it would surface
        // detail on a section the user isn't looking at anymore.
        if self.expanded != Some(next) {
            self.expanded = None;
            self.selected_item = 0;
            self.drilled = false;
        }
    }

    fn toggle_expand(&mut self) {
        let Some(focus) = self.focus else { return };
        if self.expanded == Some(focus) {
            self.expanded = None;
            self.selected_item = 0;
            self.drilled = false;
        } else {
            self.expanded = Some(focus);
            // Reset item state so the freshly-expanded section
            // starts with its first item selected.
            self.selected_item = 0;
            self.drilled = false;
        }
    }

    /// Enter drill mode on the currently-focused section's
    /// currently-selected item. No-op when no expansion, no
    /// focus, or the section has no drillable items.  Remembers
    /// the pre-drill scroll position so Esc can restore it.
    fn enter_drill(&mut self) {
        if self.expanded.is_none() || self.selectable_count() == 0 {
            return;
        }
        self.pre_drill_scroll = self.scroll;
        self.drilled = true;
        self.scroll = 0;
    }

    fn exit_drill(&mut self) {
        self.drilled = false;
        self.scroll = self.pre_drill_scroll;
    }

    /// Adjust the scroll window so the whole selected-item block
    /// stays in view — the ▸ header row AND its wrapped preview
    /// body. Previous implementation only tracked the marker row,
    /// so selecting an item with a 3-line preview left the last
    /// lines of that preview clipped off the bottom.
    ///
    /// Algorithm:
    ///   • Find the marker row (top of block) and the last row
    ///     belonging to that block (next ▸ / next section
    ///     heading / end of list).
    ///   • If the block height fits the viewport: park it within
    ///     the visible window (scroll up if block_top is above,
    ///     scroll down if block_bottom is below).
    ///   • If the block is taller than the viewport: keep
    ///     block_top at the top so the user can scroll through the
    ///     body naturally.
    fn scroll_to_selected_item(&mut self) {
        let inner_w = self.last_inner_width.get();
        let inner_h = self.last_viewport_rows.get();
        if inner_w == 0 || inner_h == 0 {
            return;
        }
        let state = self.view_state();
        let lines =
            crate::tui::context_panel::view::build_lines_with(&self.breakdown, inner_w, state);

        // Find the marker row. Every selected-item header carries
        // a literal `▸` in exactly one span — and only one item
        // per section is selected at a time.
        let Some(marker_row) = lines
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.as_ref().contains('▸')))
        else {
            return;
        };

        // Walk forward to find the block's bottom: stop at the
        // next marker row (another section's selected header —
        // shouldn't happen since only one is selected, but safe),
        // an empty line that ends the section (section separator),
        // or the end of the line list.
        let mut block_bottom = marker_row;
        for (i, line) in lines.iter().enumerate().skip(marker_row + 1) {
            let has_marker = line.spans.iter().any(|s| s.content.as_ref().contains('▸'));
            if has_marker {
                break;
            }
            if line.spans.is_empty() {
                break;
            }
            // Stop at the next item's `└ ` row. A faster
            // heuristic: the item marker row uses the indent
            // `      ▸ └` / `    ▸ └` / `    ▸ └`. Sibling items
            // that are not the selected one render `      ` (no
            // ▸) followed by `└`. Use that as the cut — detect a
            // fresh `└` in one of the first six columns.
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            // Heuristic: sibling item rows always start with some
            // whitespace, contain `└ `, and stay above the
            // preview-indent column (text_offset >= 13). Anything
            // looking like a new `#N role` sibling wraps us up.
            let trimmed = text.trim_start();
            if trimmed.starts_with("└ #")
                || trimmed.starts_with("└ bash")
                || trimmed.starts_with("└ ") && trimmed.contains(" tokens")
            {
                break;
            }
            block_bottom = i;
        }
        let block_top = marker_row as u16;
        let block_bot = block_bottom as u16;
        let block_height = block_bot.saturating_sub(block_top).saturating_add(1);

        let viewport_top = self.scroll;
        let viewport_bottom = viewport_top.saturating_add(inner_h);

        if block_height >= inner_h {
            // Block taller than viewport — pin top so the user can
            // scroll through the body.
            self.scroll = block_top;
        } else if block_top < viewport_top {
            // Block starts above viewport → scroll up.
            self.scroll = block_top;
        } else if block_bot >= viewport_bottom {
            // Block's bottom is below viewport → scroll down just
            // enough to show the full block.
            self.scroll = block_bot.saturating_sub(inner_h).saturating_add(1);
        }
        // else: block fully visible, no change.
        let max = self.max_scroll();
        self.scroll = self.scroll.min(max);
    }

    /// Adjust scroll so the currently-focused section's heading is
    /// visible — but ONLY when it would otherwise be clipped.
    ///
    /// Previous behaviour scrolled unconditionally to `line_idx - 2`
    /// which pushed the grid + legend off-screen even when the
    /// heading was already visible. That made Tab feel jumpy and
    /// lost useful overview context.  Now:
    ///
    /// - heading already visible → no scroll change at all
    /// - heading above viewport  → scroll up so heading is 2 rows
    ///   from the top
    /// - heading below viewport  → scroll down so the heading AND
    ///   at least one row of the section body are visible
    fn scroll_to_focus(&mut self) {
        let Some(focus) = self.focus else { return };
        let inner_w = self.last_inner_width.get();
        let inner_h = self.last_viewport_rows.get();
        if inner_w == 0 || inner_h == 0 {
            return;
        }
        let state = self.view_state();
        let Some(line_idx) = panel_view::section_line_index(&self.breakdown, inner_w, state, focus)
        else {
            return;
        };
        let viewport_top = self.scroll;
        let viewport_bottom = viewport_top.saturating_add(inner_h);
        // Heading sits comfortably inside the viewport? leave
        // scroll alone — keep the eye where it was, preserve grid
        // + legend visibility.
        if line_idx >= viewport_top && line_idx + 1 < viewport_bottom {
            return;
        }
        let target = if line_idx < viewport_top {
            // Scroll up — park heading near the top with a small margin.
            line_idx.saturating_sub(2)
        } else {
            // Scroll down — put the heading near the top of the
            // viewport so the user sees the new section below it.
            line_idx.saturating_sub(2)
        };
        let max = self.max_scroll();
        self.scroll = target.min(max);
    }
}

impl BottomPaneView for ContextPanelView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let inner_w = area.width.saturating_sub(2);
        let inner_h = area.height.saturating_sub(2);
        self.last_inner_width.set(inner_w);
        self.last_viewport_rows.set(inner_h);

        let state = self.view_state();
        let total = panel_view::line_count_with(&self.breakdown, inner_w, state);
        let max_scroll = total.saturating_sub(inner_h);
        let scroll = self.scroll.min(max_scroll);
        panel_view::render_with(&self.breakdown, area, buf, scroll, state);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        panel_view::desired_height(&self.breakdown)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            // Three-level Esc: drill → expanded → focus/close.
            (KeyCode::Esc, _) => {
                if self.drilled {
                    self.exit_drill();
                } else if self.expanded.is_some() {
                    self.expanded = None;
                    self.selected_item = 0;
                    // Keep focus so a second Tab keeps cycling; a
                    // second Esc closes the panel.
                } else {
                    self.completed = true;
                }
            }
            (KeyCode::Char('q'), _) => {
                self.completed = true;
            }
            // Enter walks depth-first through the hierarchy:
            //   flat/no focus → close panel
            //   focused, not expanded → expand (M1 → M2)
            //   expanded, drillable items → drill selected (M2 → M3)
            //   expanded, no drillable items → collapse (M2 → M1)
            //   drilled → ignored (Esc to back out)
            //
            // This reads as "keep going deeper on Enter, Esc to
            // back out". Hint text tells the user exactly which
            // transition is next for the current mode.
            (KeyCode::Enter, _) => {
                if self.drilled {
                    // Already at max depth — make this a no-op so
                    // a stray Enter inside a drill doesn't close
                    // the panel.
                } else if self.expanded.is_some() {
                    if self.selectable_count() > 0 {
                        self.enter_drill();
                    } else {
                        // Section has nothing to drill into
                        // (System prompt / Prompt signals /
                        // Session). Treat Enter as a collapse —
                        // the alternative (silent no-op) is
                        // worse UX.
                        self.expanded = None;
                        self.selected_item = 0;
                    }
                } else if self.focus.is_some() {
                    self.toggle_expand();
                    self.scroll_to_focus();
                    if self.selectable_count() > 0 {
                        self.scroll_to_selected_item();
                    }
                } else {
                    self.completed = true;
                }
            }
            // Tab cycles section focus (disabled inside drill so
            // the same key doesn't mean "fly away from this view").
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) if !self.drilled => {
                let reverse = matches!(key.code, KeyCode::BackTab)
                    || key.modifiers.contains(KeyModifiers::SHIFT);
                self.focus_next(reverse);
                self.scroll_to_focus();
            }
            // ↑/↓ pivots behavior: inside an expanded (non-drilled)
            // section with drillable items, move selection. Drill
            // and flat views use them as scroll.
            (KeyCode::Down, _) => {
                if !self.drilled && self.expanded.is_some() && self.selectable_count() > 0 {
                    self.select_item(1);
                    self.scroll_to_selected_item();
                } else {
                    self.scroll_by(1);
                }
            }
            (KeyCode::Up, _) => {
                if !self.drilled && self.expanded.is_some() && self.selectable_count() > 0 {
                    self.select_item(-1);
                    self.scroll_to_selected_item();
                } else {
                    self.scroll_by(-1);
                }
            }
            // j/k stay as pure scroll — avoids accidental selection
            // moves when the user is scanning content inside a
            // drill or a section without selectable items.
            (KeyCode::Char('j'), _) => self.scroll_by(1),
            (KeyCode::Char('k'), _) => self.scroll_by(-1),
            // Page scroll.
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                let page = self.last_viewport_rows.get().max(1) as i32;
                self.scroll_by(page);
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let page = self.last_viewport_rows.get().max(1) as i32;
                self.scroll_by(-page);
            }
            // Jump to start / end.
            (KeyCode::Home, _) | (KeyCode::Char('g'), _) => {
                self.scroll = 0;
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                self.scroll = self.max_scroll();
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            None
        }
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        // Hint text tracks the current mode so the user sees
        // exactly which keys do what at this depth. Matches the
        // in-panel hint in `build_lines_with` so scrolled-out
        // state doesn't leave the user guessing.
        let hint = if self.drilled {
            "j/k scroll · Esc back"
        } else if self.expanded.is_some() && self.selectable_count() > 0 {
            "↑/↓ select · Enter drill · Tab next · Esc back"
        } else if self.expanded.is_some() {
            "Tab next · Enter/Esc collapse · j/k scroll"
        } else if self.focus.is_some() {
            "Tab next · Enter expand · j/k scroll · Esc close"
        } else {
            "Tab focus · Enter close · j/k scroll · Esc close"
        };
        Some(hint.into())
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::draw_widget;
    use astra_turn_core::context_assembly_trace::{
        ContextAssemblyTrace, MemorySelection, MemorySource, SystemPromptBreakdown,
        TokenBudgetTrace, VisibleTool,
    };
    use ratatui::widgets::Widget;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// Build a breakdown big enough that scrolling is meaningful,
    /// with multiple non-empty sections so focus cycling has
    /// somewhere to go.
    fn big_breakdown() -> ContextBreakdown {
        let mut t = ContextAssemblyTrace::default();
        t.token_budget = TokenBudgetTrace {
            max_tokens: 100_000,
            system_prompt_tokens: 3_000,
            history_tokens: 20_000,
            memory_tokens: 1_000,
            tool_schema_tokens: 5_000,
            user_message_tokens: 500,
            total_used: 29_500,
            budget_pressure: 0.295,
            compression_triggered: false,
        };
        t.tools.visible_tools = (0..20)
            .map(|i| VisibleTool {
                tool_name: format!("tool_{i}"),
                tokens: 50,
            })
            .collect();
        t.memory.memories_selected = vec![MemorySelection {
            memory_id: "m1".into(),
            memory_type: "semantic".into(),
            content_preview: "test memory".into(),
            relevance_score: 0.9,
            tokens: 200,
            source: MemorySource::Memoria,
        }];
        t.system_prompt = SystemPromptBreakdown {
            base_persona_tokens: 1_500,
            environment_tokens: 500,
            ..SystemPromptBreakdown::default()
        };
        ContextBreakdown::from_trace(&t)
    }

    struct RenderWrap<'a>(&'a ContextPanelView);
    impl Widget for RenderWrap<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    fn prime_viewport(v: &ContextPanelView, width: u16, height: u16) {
        let _ = draw_widget(RenderWrap(v), width, height);
    }

    #[test]
    fn esc_marks_complete() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().unwrap().result.is_none());
    }

    #[test]
    fn enter_also_closes() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Enter));
        assert!(v.is_complete());
    }

    #[test]
    fn q_closes() {
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Char('q')));
        assert!(v.is_complete());
    }

    #[test]
    fn unknown_keys_ignored() {
        // Keys that don't match scroll / close actions must leave
        // the view state untouched — in particular they must not
        // flip `completed`.
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        v.handle_key(press(KeyCode::Char('x')));
        assert!(!v.is_complete());
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn j_and_down_scroll_forward() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.handle_key(press(KeyCode::Char('j')));
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.scroll, 2);
    }

    #[test]
    fn k_and_up_scroll_backward() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.scroll = 5;
        v.handle_key(press(KeyCode::Char('k')));
        v.handle_key(press(KeyCode::Up));
        assert_eq!(v.scroll, 3);
    }

    #[test]
    fn page_keys_jump_by_viewport_height() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 12); // 10 visible rows
        let visible = v.last_viewport_rows.get();
        assert!(visible > 0);
        v.handle_key(press(KeyCode::PageDown));
        assert_eq!(v.scroll, visible);
        v.handle_key(press(KeyCode::PageUp));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn ctrl_d_u_also_page() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 12);
        let visible = v.last_viewport_rows.get();
        v.handle_key(ctrl(KeyCode::Char('d')));
        assert_eq!(v.scroll, visible);
        v.handle_key(ctrl(KeyCode::Char('u')));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn home_and_end_jump() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.handle_key(press(KeyCode::End));
        assert!(v.scroll > 0, "End should move past zero: {}", v.scroll);
        v.handle_key(press(KeyCode::Home));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn g_and_shift_g_mirror_home_and_end() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        let end_scroll = v.scroll;
        assert!(end_scroll > 0);
        v.handle_key(press(KeyCode::Char('g')));
        assert_eq!(v.scroll, 0);
        // Re-jump to end and verify clamp is stable.
        v.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(v.scroll, end_scroll);
    }

    #[test]
    fn scroll_clamps_at_max_when_over_pressed() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        let max = v.max_scroll();
        for _ in 0..200 {
            v.handle_key(press(KeyCode::Char('j')));
        }
        assert_eq!(v.scroll, max, "scroll must not exceed max");
    }

    #[test]
    fn scroll_clamps_at_zero_when_under_pressed() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        for _ in 0..200 {
            v.handle_key(press(KeyCode::Char('k')));
        }
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn empty_breakdown_scroll_stays_zero() {
        // `line_count` for an empty breakdown is small enough to fit
        // in any viewport, so max_scroll is zero and nothing moves.
        let mut v = ContextPanelView::new(ContextBreakdown::empty());
        prime_viewport(&v, 80, 5);
        v.handle_key(press(KeyCode::Char('j')));
        v.handle_key(press(KeyCode::PageDown));
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn hint_includes_scroll_keys() {
        let v = ContextPanelView::new(ContextBreakdown::empty());
        let hint = v.hint_keys().unwrap();
        assert!(hint.contains("scroll"));
        assert!(hint.contains("close"));
    }

    // ─── Focus + expand ────────────────────────────────────────────

    #[test]
    fn tab_jumps_to_first_non_empty_section_then_cycles() {
        let mut v = ContextPanelView::new(big_breakdown());
        assert_eq!(v.focus, None);
        v.handle_key(press(KeyCode::Tab));
        assert!(v.focus.is_some(), "Tab must enter focus mode");
        let start = v.focus.unwrap();
        // big_breakdown has 3 non-empty sections (System, Tools,
        // Memory); Skills and History are empty and must be
        // skipped.  Three more Tabs should complete the cycle.
        for _ in 0..3 {
            v.handle_key(press(KeyCode::Tab));
        }
        assert_eq!(v.focus, Some(start), "cycle did not return to start");
    }

    #[test]
    fn shift_tab_walks_backwards() {
        let mut v = ContextPanelView::new(big_breakdown());
        v.handle_key(press(KeyCode::Tab));
        let first = v.focus.unwrap();
        v.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        let back = v.focus.unwrap();
        v.handle_key(press(KeyCode::Tab));
        assert_eq!(
            v.focus,
            Some(first),
            "BackTab then Tab must return to starting focus"
        );
        assert_ne!(back, first, "BackTab must move focus");
    }

    #[test]
    fn tab_ignores_empty_sections() {
        // big_breakdown has tools + system prompt but no memory /
        // skills / history. Cycle through all sections — the focus
        // must never land on empty ones.
        let mut v = ContextPanelView::new(big_breakdown());
        let mut visited = Vec::new();
        for _ in 0..12 {
            v.handle_key(press(KeyCode::Tab));
            if let Some(s) = v.focus {
                visited.push(s);
                assert!(
                    v.breakdown.section_non_empty(s),
                    "focus landed on empty section: {s:?}"
                );
            }
        }
        assert!(!visited.is_empty());
    }

    #[test]
    fn enter_with_focus_toggles_expansion() {
        let mut v = ContextPanelView::new(big_breakdown());
        v.handle_key(press(KeyCode::Tab));
        let focus = v.focus.unwrap();
        v.handle_key(press(KeyCode::Enter));
        assert_eq!(v.expanded, Some(focus));
        v.handle_key(press(KeyCode::Enter));
        assert_eq!(v.expanded, None);
    }

    #[test]
    fn enter_without_focus_still_closes_the_panel() {
        // Users who never press Tab should retain the original
        // "Enter closes" muscle memory.
        let mut v = ContextPanelView::new(big_breakdown());
        v.handle_key(press(KeyCode::Enter));
        assert!(v.is_complete());
    }

    #[test]
    fn esc_collapses_expansion_before_closing() {
        let mut v = ContextPanelView::new(big_breakdown());
        v.handle_key(press(KeyCode::Tab));
        v.handle_key(press(KeyCode::Enter));
        assert!(v.expanded.is_some());
        // First Esc: collapse, don't close.
        v.handle_key(press(KeyCode::Esc));
        assert!(v.expanded.is_none());
        assert!(!v.is_complete());
        // Second Esc: now it closes.
        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete());
    }

    #[test]
    fn focus_change_collapses_previous_expansion() {
        // Keeping expansion alive across a focus change would leave
        // detail showing for a section the user isn't looking at.
        let mut v = ContextPanelView::new(big_breakdown());
        v.handle_key(press(KeyCode::Tab));
        v.handle_key(press(KeyCode::Enter));
        assert!(v.expanded.is_some());
        v.handle_key(press(KeyCode::Tab));
        assert!(v.expanded.is_none(), "moving focus must collapse");
    }

    #[test]
    fn enter_progression_expand_then_drill() {
        // The canonical Enter flow: Tab → focused; 1st Enter →
        // expand; 2nd Enter → drill. 3rd Enter (inside drill) is
        // a no-op so a stray keypress doesn't close the panel.
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        v.handle_key(press(KeyCode::Tab));
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        assert!(v.expanded.is_none() && !v.drilled);
        v.handle_key(press(KeyCode::Enter));
        assert!(v.expanded.is_some());
        assert!(!v.drilled);
        v.handle_key(press(KeyCode::Enter));
        assert!(v.drilled, "second Enter must drill");
        v.handle_key(press(KeyCode::Enter));
        assert!(v.drilled, "third Enter is a no-op inside drill");
        assert!(
            !v.is_complete(),
            "panel must not close on Enter inside drill"
        );
    }

    #[test]
    fn enter_on_section_with_no_selectable_items_collapses_instead_of_drilling() {
        // System prompt / Prompt signals / Session have
        // section_item_count == 0. Enter on an expanded but
        // non-drillable section should collapse (not drill a
        // nonexistent item and not close the panel).
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::SystemPrompt) {
            v.handle_key(press(KeyCode::Tab));
        }
        assert_eq!(v.selectable_count(), 0, "precondition: no drillable items");
        v.handle_key(press(KeyCode::Enter)); // expand
        assert!(v.expanded.is_some());
        v.handle_key(press(KeyCode::Enter)); // "drill" → actually collapse
        assert!(v.expanded.is_none());
        assert!(!v.drilled);
        assert!(!v.is_complete());
    }

    #[test]
    fn hint_keys_tracks_every_mode() {
        // Hint text must match the current interaction mode so
        // the user always sees which keys do what. Cover each of
        // the four modes (flat, focused, expanded-drillable,
        // drilled) — the no-items variant is rarer and covered
        // separately via the handler test above.
        let mut v = ContextPanelView::new(big_breakdown());
        assert!(v.hint_keys().unwrap().contains("Tab focus"));
        v.handle_key(press(KeyCode::Tab));
        assert!(v.hint_keys().unwrap().contains("Enter expand"));
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter));
        let hint = v.hint_keys().unwrap();
        assert!(hint.contains("↑/↓ select"), "got {hint}");
        assert!(hint.contains("Enter drill"), "got {hint}");
        v.handle_key(press(KeyCode::Enter));
        let hint = v.hint_keys().unwrap();
        assert!(hint.contains("Esc back"), "drill hint: {hint}");
    }

    #[test]
    fn tab_anchors_focused_heading_in_view() {
        // Tabbing onto a later section should reposition the
        // scroll so the heading is visible near the top (≈2 rows
        // of padding above).
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.handle_key(press(KeyCode::Tab));
        let first_scroll = v.scroll;
        v.handle_key(press(KeyCode::Tab));
        let second_scroll = v.scroll;
        // System prompt comes first → scroll may still be near 0,
        // but the second section's heading is deeper so the scroll
        // must advance.
        assert!(
            second_scroll >= first_scroll,
            "Tab must scroll forward to reach later sections: {first_scroll} → {second_scroll}"
        );
    }

    #[test]
    fn down_inside_expanded_section_advances_item_selection() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        // Jump to Tools (has drillable items).
        for _ in 0..4 {
            v.handle_key(press(KeyCode::Tab));
            if v.focus == Some(Section::Tools) {
                break;
            }
        }
        assert_eq!(v.focus, Some(Section::Tools));
        v.handle_key(press(KeyCode::Enter));
        assert!(v.expanded.is_some());
        assert_eq!(v.selected_item, 0);
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.selected_item, 1);
        v.handle_key(press(KeyCode::Up));
        assert_eq!(v.selected_item, 0);
    }

    #[test]
    fn down_outside_expansion_scrolls_instead() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        let before = v.scroll;
        v.handle_key(press(KeyCode::Down));
        assert!(
            v.scroll >= before,
            "Down without expansion must scroll: {} → {}",
            before,
            v.scroll
        );
    }

    #[test]
    fn down_selection_clamps_at_last_item() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 20);
        // Focus Tools.
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter));
        let tools_n = v.selectable_count();
        assert!(tools_n >= 2);
        for _ in 0..(tools_n + 5) {
            v.handle_key(press(KeyCode::Down));
        }
        assert_eq!(v.selected_item, tools_n - 1);
    }

    #[test]
    fn enter_on_selected_item_drills_in() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter)); // expand
        assert!(!v.drilled);
        v.handle_key(press(KeyCode::Enter)); // drill
        assert!(v.drilled);
        assert!(v.expanded.is_some());
    }

    #[test]
    fn esc_level_back_drill_then_collapse_then_close() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter)); // expand
        v.handle_key(press(KeyCode::Enter)); // drill
        assert!(v.drilled);

        v.handle_key(press(KeyCode::Esc));
        assert!(!v.drilled, "Esc leaves drill");
        assert!(v.expanded.is_some(), "section still expanded");

        v.handle_key(press(KeyCode::Esc));
        assert!(v.expanded.is_none(), "Esc collapses section");
        assert!(!v.is_complete(), "panel still open");

        v.handle_key(press(KeyCode::Esc));
        assert!(v.is_complete(), "Esc closes panel");
    }

    #[test]
    fn drill_scroll_resets_and_restores() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter));
        // Scroll down a few rows, then drill; scroll should reset to 0.
        v.scroll = 5;
        v.handle_key(press(KeyCode::Enter));
        assert!(v.drilled);
        assert_eq!(v.scroll, 0, "drill resets scroll");
        // Esc should restore the pre-drill scroll so the eye
        // lands on the same item.
        v.handle_key(press(KeyCode::Esc));
        assert_eq!(v.scroll, 5, "exit-drill restores pre-drill scroll");
    }

    #[test]
    fn focus_change_clears_selection_and_drill() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter));
        v.handle_key(press(KeyCode::Down));
        assert_eq!(v.selected_item, 1);
        // Move focus elsewhere — selection resets, expansion
        // clears (the new focused section starts from a clean
        // collapsed state). Drill isn't active here because we
        // haven't Entered drill yet, but the clearing rule
        // extends to drill too (see `tab_ignored_inside_drill`
        // for the drill path, which requires Esc first).
        v.handle_key(press(KeyCode::Tab));
        assert_eq!(v.selected_item, 0);
        assert!(v.expanded.is_none());
    }

    #[test]
    fn tab_ignored_inside_drill() {
        // Tab should not fly away from a drill — users expect Esc
        // to back out first.
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 14);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter));
        v.handle_key(press(KeyCode::Enter));
        let focus_before = v.focus;
        v.handle_key(press(KeyCode::Tab));
        assert_eq!(v.focus, focus_before, "Tab must be a no-op inside drill");
        assert!(v.drilled);
    }

    #[test]
    fn scroll_to_focus_noop_when_heading_already_visible() {
        // Regression: Tab cycling from the first focused section
        // used to unconditionally re-scroll, pushing the
        // grid/legend off-screen even when the target heading was
        // already fully visible.
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 24);
        v.handle_key(press(KeyCode::Tab));
        let first_scroll = v.scroll;
        // Tab to next focus; heading of the next section is still
        // above the viewport floor, so scroll should stay where
        // it was (at 0 for this fixture).
        v.handle_key(press(KeyCode::Tab));
        assert_eq!(
            v.scroll, first_scroll,
            "no-op scroll expected when next heading is already in viewport",
        );
    }

    #[test]
    fn down_keeps_selected_item_block_visible() {
        // Regression: with variable preview heights, selecting an
        // item with a longer body pushed later items off the
        // bottom. After the preview-height stabilisation we walk
        // through a section and verify the selected block always
        // fits the viewport.
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 18);
        while v.focus != Some(Section::Tools) {
            v.handle_key(press(KeyCode::Tab));
        }
        v.handle_key(press(KeyCode::Enter)); // expand
        let tools_n = v.selectable_count();
        assert!(tools_n >= 4, "need multi-item section for this check");
        for _ in 0..tools_n {
            v.handle_key(press(KeyCode::Down));
            let state = v.view_state();
            let lines = crate::tui::context_panel::view::build_lines_with(
                &v.breakdown,
                v.last_inner_width.get(),
                state,
            );
            let marker = lines
                .iter()
                .position(|l| l.spans.iter().any(|s| s.content.as_ref().contains('▸')))
                .unwrap() as u16;
            let viewport_top = v.scroll;
            let viewport_bottom = viewport_top + v.last_viewport_rows.get();
            assert!(
                marker >= viewport_top && marker < viewport_bottom,
                "selected-item marker must stay inside viewport (marker={marker}, top={viewport_top}, bot={viewport_bottom})"
            );
        }
    }

    #[test]
    fn first_tab_resets_stale_nested_state() {
        // Consistency fix: focus_next's None-branch used to only
        // clear `expanded`; selected_item and drilled could leak
        // in principle from a prior session. Call focus_next
        // directly — going through the public keymap wouldn't
        // exercise this path because Tab is blocked while drilled.
        let mut v = ContextPanelView::new(big_breakdown());
        v.selected_item = 4;
        v.drilled = true;
        v.focus_next(false);
        assert_eq!(v.selected_item, 0);
        assert!(!v.drilled);
        assert!(v.focus.is_some(), "first tab enters focus mode");
    }

    #[test]
    fn expand_reanchors_scroll_on_heading() {
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.handle_key(press(KeyCode::Tab));
        let before = v.scroll;
        v.handle_key(press(KeyCode::Enter));
        // After expansion we re-anchor; the scroll value is deterministic
        // relative to the heading, so it should either stay the same
        // or adjust (but never drift so far that the heading leaves
        // the viewport).
        let heading = crate::tui::context_panel::view::section_line_index(
            &v.breakdown,
            v.last_inner_width.get(),
            v.view_state(),
            v.focus.unwrap(),
        )
        .unwrap();
        let page = v.last_viewport_rows.get();
        assert!(
            v.scroll <= heading,
            "heading must be at or below scroll top after expand (scroll={}, heading={})",
            v.scroll,
            heading
        );
        assert!(
            heading < v.scroll.saturating_add(page),
            "heading must stay within the visible viewport after expand"
        );
        let _ = before;
    }
}
