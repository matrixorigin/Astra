use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use super::textarea::{TextArea, TextAreaAction};

/// Duration the `›` prefix glows after the user submits a message.
/// Short enough to feel instantaneous, long enough to be noticed at a
/// glance even when the next frame arrives quickly.
const SUBMIT_FLASH_DURATION: Duration = Duration::from_millis(180);

#[derive(Debug)]
pub(crate) struct ChatComposer {
    textarea: TextArea,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: Option<String>,
    prompt_prefix: String,
    /// Pasted blobs hidden behind short `[Pasted #N · M lines]`
    /// placeholders inside the textarea. On submit, placeholders are
    /// expanded back to the original text. Keeps the composer visually
    /// compact when users paste files, logs, or stack traces.
    pasted_blobs: Vec<(String, String)>,
    paste_counter: u32,
    /// Wall-clock instant of the most recent submit. When within
    /// `SUBMIT_FLASH_DURATION`, the `›` prefix renders in an accent
    /// color so the user gets instant visual feedback that the message
    /// was accepted.
    last_submit_at: Option<Instant>,
}

/// Multi-line pastes above this threshold are swapped for a placeholder.
/// Single-line pastes are inserted literally regardless of length — users
/// expect URLs and one-liners to appear verbatim.
const PASTE_PLACEHOLDER_MIN_LINES: usize = 4;

impl ChatComposer {
    pub fn new() -> Self {
        Self {
            textarea: TextArea::new(),
            history: Vec::new(),
            history_index: None,
            draft: None,
            prompt_prefix: "› ".to_string(),
            pasted_blobs: Vec::new(),
            paste_counter: 0,
            last_submit_at: None,
        }
    }

    /// True while the submit-flash animation should paint the prefix in
    /// the accent color. Callers (tests + render) should treat this as
    /// a monotonically-decaying flag based on wall clock.
    pub fn is_flashing(&self) -> bool {
        self.is_flashing_at(Instant::now())
    }

    fn is_flashing_at(&self, now: Instant) -> bool {
        match self.last_submit_at {
            Some(t) => now.duration_since(t) < SUBMIT_FLASH_DURATION,
            None => false,
        }
    }

    pub fn text(&self) -> String {
        self.textarea.text().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    pub fn clear_and_submit(&mut self) -> String {
        let raw = self.textarea.text().to_string();
        let expanded = self.expand_pastes(&raw);
        if !expanded.trim().is_empty() {
            self.history.push(expanded.clone());
        }
        self.textarea.clear();
        self.history_index = None;
        self.draft = None;
        self.pasted_blobs.clear();
        // Non-empty submits trigger a brief prefix flash. Empty submits
        // should be no-ops visually — BottomPane already blocks them at
        // the ComposerAction layer, but guard here too so future
        // callers don't accidentally flash on nothing.
        if !expanded.is_empty() {
            self.last_submit_at = Some(Instant::now());
        }
        expanded
    }

    pub fn clear_draft(&mut self) {
        self.textarea.clear();
        self.history_index = None;
        self.draft = None;
        self.pasted_blobs.clear();
    }

    /// Handle a bracketed-paste event. Multi-line pastes are collapsed to
    /// a `[Pasted #N · M lines]` placeholder; short pastes go in verbatim.
    /// Returns `true` when a placeholder was inserted.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let line_count = text.lines().count();
        if line_count < PASTE_PLACEHOLDER_MIN_LINES {
            self.textarea.insert_str(text);
            return false;
        }
        self.paste_counter += 1;
        let placeholder = format!("[Pasted #{} · {} lines]", self.paste_counter, line_count);
        self.pasted_blobs.push((placeholder.clone(), text.to_string()));
        self.textarea.insert_str(&placeholder);
        true
    }

    /// Replace every placeholder in `text` with its stored blob.
    fn expand_pastes(&self, text: &str) -> String {
        if self.pasted_blobs.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (placeholder, blob) in &self.pasted_blobs {
            out = out.replace(placeholder, blob);
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn pasted_blob_count(&self) -> usize {
        self.pasted_blobs.len()
    }

    #[cfg(test)]
    pub(crate) fn mark_submit_at_for_test(&mut self, t: Instant) {
        self.last_submit_at = Some(t);
    }

    #[cfg(test)]
    pub(crate) fn is_flashing_at_for_test(&self, now: Instant) -> bool {
        self.is_flashing_at(now)
    }

    pub fn set_text(&mut self, text: &str) {
        self.textarea.set_text(text);
    }

    /// Current cursor byte offset within `text()`.
    pub fn cursor_byte(&self) -> usize {
        self.textarea.cursor_byte()
    }

    fn prefix_display_width(&self) -> u16 {
        self.prompt_prefix.width() as u16
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let inner_w = width.saturating_sub(self.prefix_display_width());
        self.textarea.desired_height(inner_w)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ComposerAction {
        match self.textarea.handle_key(key) {
            TextAreaAction::Submit => {
                if self.textarea.is_empty() {
                    ComposerAction::Consumed
                } else {
                    ComposerAction::Submit
                }
            }
            TextAreaAction::Cancel => {
                if !self.is_empty() {
                    self.clear_draft();
                    ComposerAction::Consumed
                } else {
                    ComposerAction::Interrupt
                }
            }
            TextAreaAction::Quit => ComposerAction::Quit,
            TextAreaAction::HistoryPrev => {
                self.navigate_history_prev();
                ComposerAction::Consumed
            }
            TextAreaAction::HistoryNext => {
                self.navigate_history_next();
                ComposerAction::Consumed
            }
            TextAreaAction::Changed | TextAreaAction::Consumed => ComposerAction::Consumed,
            TextAreaAction::Unhandled => ComposerAction::Unhandled,
        }
    }

    fn navigate_history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = Some(self.textarea.text().to_string());
                self.history_index = Some(self.history.len() - 1);
            }
            Some(0) => return,
            Some(i) => {
                self.history_index = Some(i - 1);
            }
        }
        if let Some(i) = self.history_index {
            self.textarea.set_text(&self.history[i]);
        }
    }

    fn navigate_history_next(&mut self) {
        match self.history_index {
            None => (),
            Some(i) if i + 1 >= self.history.len() => {
                self.history_index = None;
                if let Some(ref draft) = self.draft.take() {
                    self.textarea.set_text(draft);
                } else {
                    self.textarea.clear();
                }
            }
            Some(i) => {
                self.history_index = Some(i + 1);
                self.textarea.set_text(&self.history[i + 1]);
            }
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        // Codex: › bold when active. While a submit flash is active,
        // paint the prefix in the theme accent — gives users a quick
        // "message accepted" cue that doesn't require reading the
        // scrollback.
        let mut prefix_style =
            Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        if self.is_flashing() {
            let theme = crate::tui::theme::current();
            prefix_style = prefix_style.fg(theme.accent);
        }
        let prefix = Span::styled(&self.prompt_prefix, prefix_style);
        let prefix_width = self.prefix_display_width();
        let prefix_area = Rect::new(area.x, area.y, prefix_width.min(area.width), 1);
        Widget::render(Line::from(prefix), prefix_area, buf);

        let text_area = Rect::new(
            area.x + prefix_width.min(area.width),
            area.y,
            area.width.saturating_sub(prefix_width),
            area.height,
        );

        if self.textarea.is_empty() {
            let placeholder = Span::styled(
                "Ask astra to do anything",
                Style::default().fg(Color::DarkGray),
            );
            Widget::render(Line::from(placeholder), text_area, buf);
        } else {
            self.textarea.render(text_area, buf);
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let prefix_width = self.prefix_display_width();
        let text_area = Rect::new(
            area.x + prefix_width.min(area.width),
            area.y,
            area.width.saturating_sub(prefix_width),
            area.height,
        );
        self.textarea.cursor_position(text_area)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    Submit,
    Interrupt,
    Quit,
    Consumed,
    Unhandled,
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn short_paste_is_inserted_verbatim() {
        let mut c = ChatComposer::new();
        let placeholder_used = c.handle_paste("one liner");
        assert!(!placeholder_used);
        assert_eq!(c.text(), "one liner");
        assert_eq!(c.pasted_blob_count(), 0);
    }

    #[test]
    fn two_line_paste_still_verbatim() {
        // Only real multi-line pastes get folded; 2-line is still short.
        let mut c = ChatComposer::new();
        c.handle_paste("a\nb");
        assert_eq!(c.text(), "a\nb");
        assert_eq!(c.pasted_blob_count(), 0);
    }

    #[test]
    fn big_paste_becomes_placeholder_and_expands_on_submit() {
        let mut c = ChatComposer::new();
        let blob = "line1\nline2\nline3\nline4\nline5";
        let used = c.handle_paste(blob);
        assert!(used, "4+ line paste should trigger the placeholder");
        let visible = c.text();
        assert!(
            visible.starts_with("[Pasted #1 · 5 lines]"),
            "composer text should show placeholder, got {visible:?}"
        );

        let submitted = c.clear_and_submit();
        assert_eq!(submitted, blob, "submit must expand back to original");
        assert_eq!(c.pasted_blob_count(), 0, "blob table cleared on submit");
    }

    #[test]
    fn multiple_big_pastes_get_unique_placeholders_and_both_expand() {
        let mut c = ChatComposer::new();
        c.handle_paste("a1\na2\na3\na4");
        c.set_text(&format!("{} prefix ", c.text())); // sanity: can edit around placeholder
        c.handle_paste("b1\nb2\nb3\nb4\nb5");

        let submitted = c.clear_and_submit();
        assert!(submitted.contains("a1\na2\na3\na4"));
        assert!(submitted.contains("b1\nb2\nb3\nb4\nb5"));
        // Placeholders must not leak into the submitted payload.
        assert!(!submitted.contains("[Pasted"));
    }

    #[test]
    fn submit_triggers_flash_then_decays() {
        let mut c = ChatComposer::new();
        assert!(!c.is_flashing(), "fresh composer never flashes");

        c.set_text("hello");
        let _ = c.clear_and_submit();

        let t0 = c.last_submit_at.expect("submit should stamp an instant");
        assert!(c.is_flashing_at_for_test(t0), "flash is on immediately after submit");
        assert!(
            c.is_flashing_at_for_test(t0 + SUBMIT_FLASH_DURATION / 2),
            "still flashing halfway through"
        );
        assert!(
            !c.is_flashing_at_for_test(t0 + SUBMIT_FLASH_DURATION),
            "flash has decayed at SUBMIT_FLASH_DURATION"
        );
        assert!(
            !c.is_flashing_at_for_test(t0 + SUBMIT_FLASH_DURATION + Duration::from_millis(50)),
            "flash is off well after the window"
        );
    }

    #[test]
    fn empty_submit_does_not_flash() {
        let mut c = ChatComposer::new();
        // Empty textarea → clear_and_submit returns empty and should not
        // arm the flash (BottomPane also guards this upstream, but we
        // belt-and-suspenders it so accidental empty submits stay quiet).
        let out = c.clear_and_submit();
        assert!(out.is_empty());
        assert!(!c.is_flashing());
    }

    #[test]
    fn clear_draft_drops_blobs() {
        let mut c = ChatComposer::new();
        c.handle_paste("1\n2\n3\n4\n5\n");
        assert_eq!(c.pasted_blob_count(), 1);
        c.clear_draft();
        assert_eq!(c.pasted_blob_count(), 0);
        assert!(c.text().is_empty());
    }
}
