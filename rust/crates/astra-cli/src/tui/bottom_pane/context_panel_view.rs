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
            last_viewport_rows: Cell::new(0),
            last_inner_width: Cell::new(80),
        }
    }

    fn view_state(&self) -> panel_view::ViewState {
        panel_view::ViewState {
            focus: self.focus,
            expanded: self.expanded,
        }
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
                // Collapse any prior expansion — focus just landed
                // on a new (possibly different) section.
                if let Some(exp) = self.expanded
                    && Some(exp) != self.focus
                {
                    self.expanded = None;
                }
                return;
            }
        };
        // Walk the cycle until we hit a section with content or
        // wrap back to where we started (safety against all-empty).
        let start = current;
        let mut next = if reverse { current.prev() } else { current.next() };
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
        }
    }

    fn toggle_expand(&mut self) {
        let Some(focus) = self.focus else { return };
        if self.expanded == Some(focus) {
            self.expanded = None;
        } else {
            self.expanded = Some(focus);
        }
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
            // Close first: Esc with nothing expanded closes the
            // panel; Esc while a section is expanded just collapses
            // it (stepwise dismiss matches editors users know).
            (KeyCode::Esc, _) => {
                if self.expanded.is_some() {
                    self.expanded = None;
                } else {
                    self.completed = true;
                }
            }
            (KeyCode::Char('q'), _) => {
                self.completed = true;
            }
            // Enter expands the focused section — or closes the
            // panel when there's no focus (preserves the old
            // "Enter closes" gesture for users who don't drill in).
            (KeyCode::Enter, _) => {
                if self.focus.is_some() {
                    self.toggle_expand();
                    // Reset scroll to the top on expand/collapse so
                    // the user lands on the section heading they
                    // just acted on instead of mid-section.
                    self.scroll = 0;
                } else {
                    self.completed = true;
                }
            }
            // Tab cycles section focus; Shift+Tab walks backwards.
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                let reverse = matches!(key.code, KeyCode::BackTab)
                    || key.modifiers.contains(KeyModifiers::SHIFT);
                self.focus_next(reverse);
                self.scroll = 0;
            }
            // Fine scroll.
            (KeyCode::Down | KeyCode::Char('j'), _) => self.scroll_by(1),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.scroll_by(-1),
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
        Some("Tab focus · Enter expand · j/k scroll · Esc close".into())
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
        TokenBudgetTrace, ToolSelected,
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
        t.tools.tools_selected = (0..20)
            .map(|i| ToolSelected {
                tool_name: format!("tool_{i}"),
                score: 0.5,
                tokens: 50,
                selection_factors: Vec::new(),
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
    fn expand_resets_scroll_to_top() {
        // Expansion grows the content; leaving the scroll position
        // where it was before would land the user mid-section.
        let mut v = ContextPanelView::new(big_breakdown());
        prime_viewport(&v, 80, 10);
        v.scroll = 4;
        v.handle_key(press(KeyCode::Tab));
        v.handle_key(press(KeyCode::Enter));
        assert_eq!(v.scroll, 0);
    }
}
