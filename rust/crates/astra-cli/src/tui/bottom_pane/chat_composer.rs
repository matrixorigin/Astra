use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::textarea::{TextArea, TextAreaAction};

/// Cap on the on-disk history so `~/.astra/history` doesn't grow
/// unbounded. The oldest entries are dropped when the file is
/// rewritten after an append crosses this threshold.
const HISTORY_MAX_ENTRIES: usize = 500;

/// Duration the `·` prefix glows after the user submits a message.
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
    /// `SUBMIT_FLASH_DURATION`, the `·` prefix renders in an accent
    /// color so the user gets instant visual feedback that the message
    /// was accepted.
    last_submit_at: Option<Instant>,
    /// On-disk history file. `None` when the home dir is undetermined
    /// (keeps the struct usable in tests without touching the FS).
    history_path: Option<PathBuf>,
}

/// Multi-line pastes above this threshold are swapped for a placeholder.
/// Single-line pastes are inserted literally regardless of length — users
/// expect URLs and one-liners to appear verbatim.
const PASTE_INLINE_MAX_CHARS: usize = 800;
const PASTE_INLINE_MAX_LINES: usize = 2;
const IDLE_COMPOSER_PLACEHOLDER: &str = "Message astra";
const ACTIVE_TURN_PLACEHOLDER: &str = "Send follow-up";

impl ChatComposer {
    pub fn new() -> Self {
        let (history, hist_path) = load_history();
        Self::build(history, hist_path)
    }

    /// In-memory-only composer for unit tests that shouldn't touch
    /// `~/.astra/history`. Avoids cross-test contamination when the
    /// suite runs in parallel with real users' history files.
    #[cfg(test)]
    pub(crate) fn new_ephemeral() -> Self {
        Self::build(Vec::new(), None)
    }

    fn build(history: Vec<String>, history_path: Option<PathBuf>) -> Self {
        Self {
            textarea: TextArea::new(),
            history,
            history_index: None,
            draft: None,
            prompt_prefix: "· ".to_string(),
            pasted_blobs: Vec::new(),
            paste_counter: 0,
            last_submit_at: None,
            history_path,
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

    /// True when the user is browsing history (Up/Down navigation).
    /// Callers use this to suppress popup menus that would capture
    /// arrow keys and block further history traversal.
    pub fn is_browsing_history(&self) -> bool {
        self.history_index.is_some()
    }

    pub fn clear_and_submit(&mut self) -> String {
        let raw = self.textarea.text().to_string();
        let expanded = self.expand_pastes(&raw);
        if !expanded.trim().is_empty() {
            // Dedup consecutive entries — typing the same command twice
            // in a row shouldn't double up the history list.
            if self.history.last() != Some(&expanded) {
                self.history.push(expanded.clone());
                self.persist_entry(&expanded);
            }
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
        // Count line breaks: \r\n counts as one break, lone \r or \n also one.
        let mut line_breaks = 0usize;
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' {
                line_breaks += 1;
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 1;
                }
            } else if bytes[i] == b'\n' {
                line_breaks += 1;
            }
            i += 1;
        }
        let line_count = if text.is_empty() { 0 } else { line_breaks + 1 };
        let char_count = text.chars().count();
        let is_small = char_count <= PASTE_INLINE_MAX_CHARS && line_count <= PASTE_INLINE_MAX_LINES;
        if is_small {
            self.textarea.insert_str(text);
            return false;
        }
        self.paste_counter += 1;
        let placeholder = format!("[Pasted #{} · {} lines]", self.paste_counter, line_count);
        self.pasted_blobs
            .push((placeholder.clone(), text.to_string()));
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

    /// Append the submitted line to the on-disk history. Multi-line
    /// entries are encoded with a `\n` escape so each file line holds
    /// exactly one entry — the same convention rustyline uses, making
    /// the format interchangeable with the non-TUI REPL.
    fn persist_entry(&self, entry: &str) {
        let Some(ref path) = self.history_path else {
            return;
        };
        let encoded = encode_entry(entry);
        // Append-only for the common case; rotate the whole file only
        // when the in-memory cache grows past the cap, to keep the
        // amortised cost O(1) per submit.
        if self.history.len() <= HISTORY_MAX_ENTRIES {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{encoded}");
            }
            return;
        }
        // Over budget: rewrite with only the newest `HISTORY_MAX_ENTRIES`.
        let keep = &self.history[self.history.len() - HISTORY_MAX_ENTRIES..];
        if let Ok(mut f) = std::fs::File::create(path) {
            for line in keep {
                let _ = writeln!(f, "{}", encode_entry(line));
            }
        }
    }
}

fn history_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".astra").join("history"))
}

fn load_history() -> (Vec<String>, Option<PathBuf>) {
    let Some(path) = history_file_path() else {
        return (Vec::new(), None);
    };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return (Vec::new(), Some(path)),
    };
    let mut out: Vec<String> = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        out.push(decode_entry(&line));
    }
    // Drop ancient entries so the in-memory cache stays bounded even
    // if the file on disk grew outside our control.
    if out.len() > HISTORY_MAX_ENTRIES {
        let drop = out.len() - HISTORY_MAX_ENTRIES;
        out.drain(0..drop);
    }
    (out, Some(path))
}

/// Encode a history entry for single-line storage. Newlines become
/// `\\n`, backslashes `\\\\` — lossless and readable by `decode_entry`.
fn encode_entry(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

fn decode_entry(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('r') => {
                    chars.next();
                    out.push('\r');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl ChatComposer {
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

    pub fn flush_paste_burst(&mut self) -> bool {
        self.textarea.flush_paste_burst()
    }

    #[cfg(test)]
    pub(crate) fn force_pending_paste_burst_for_test(
        &mut self,
        text: &str,
        now: std::time::Instant,
    ) {
        self.textarea.force_pending_paste_burst_for_test(text, now);
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        let inner_w = width.saturating_sub(self.prefix_display_width());
        self.textarea.desired_height(inner_w)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ComposerAction {
        if key.code == crossterm::event::KeyCode::Char('e')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return ComposerAction::OpenExternalEditor;
        }
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

    pub fn render(&self, area: Rect, buf: &mut Buffer, task_active: bool) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let theme = crate::tui::theme::current();
        let panel = crate::tui::style::composer_surface_style();
        fill_area(buf, area, panel);

        // Keep the composer prompt visible without shouting. The
        // submit flash still upgrades it to the full accent to signal
        // that the input was accepted.
        let mut prefix_style = Style::default()
            .fg(theme.accent_dim())
            .add_modifier(ratatui::style::Modifier::BOLD)
            .bg(panel.bg.unwrap_or(Color::Reset));
        if self.is_flashing() {
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
            let placeholder_text = if task_active {
                ACTIVE_TURN_PLACEHOLDER
            } else {
                IDLE_COMPOSER_PLACEHOLDER
            };
            let placeholder = Span::styled(
                truncate_end(placeholder_text, text_area.width as usize),
                Style::default()
                    .fg(theme.dim)
                    .bg(panel.bg.unwrap_or(Color::Reset)),
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
    OpenExternalEditor,
    Interrupt,
    Quit,
    Consumed,
    Unhandled,
}

fn fill_area(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.y + area.height {
        buf.set_string(area.x, y, " ".repeat(area.width as usize), style);
    }
}

fn truncate_end(text: &str, max_width: usize) -> String {
    let width = text.width();
    if width <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let keep = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn short_paste_is_inserted_verbatim() {
        let mut c = ChatComposer::new_ephemeral();
        let placeholder_used = c.handle_paste("one liner");
        assert!(!placeholder_used);
        assert_eq!(c.text(), "one liner");
        assert_eq!(c.pasted_blob_count(), 0);
    }

    #[test]
    fn two_line_paste_still_verbatim() {
        // Only real multi-line pastes get folded; 2-line is still short.
        let mut c = ChatComposer::new_ephemeral();
        c.handle_paste("a\nb");
        assert_eq!(c.text(), "a\nb");
        assert_eq!(c.pasted_blob_count(), 0);
    }

    #[test]
    fn big_paste_becomes_placeholder_and_expands_on_submit() {
        let mut c = ChatComposer::new_ephemeral();
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
        let mut c = ChatComposer::new_ephemeral();
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
        let mut c = ChatComposer::new_ephemeral();
        assert!(!c.is_flashing(), "fresh composer never flashes");

        c.set_text("hello");
        let _ = c.clear_and_submit();

        let t0 = c.last_submit_at.expect("submit should stamp an instant");
        assert!(
            c.is_flashing_at_for_test(t0),
            "flash is on immediately after submit"
        );
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
        let mut c = ChatComposer::new_ephemeral();
        // Empty textarea → clear_and_submit returns empty and should not
        // arm the flash (BottomPane also guards this upstream, but we
        // belt-and-suspenders it so accidental empty submits stay quiet).
        let out = c.clear_and_submit();
        assert!(out.is_empty());
        assert!(!c.is_flashing());
    }

    #[test]
    fn encode_decode_roundtrip_multiline() {
        let entry = "line1\nline2\\with\\backslashes\n";
        let encoded = encode_entry(entry);
        assert!(
            !encoded.contains('\n'),
            "encoded form must be single-line for file storage, got {encoded:?}"
        );
        assert_eq!(decode_entry(&encoded), entry);
    }

    #[serial_test::serial]
    #[test]
    #[serial_test::serial]
    fn history_persists_and_reloads() {
        let _home = crate::tests::HomeGuard::temp();

        {
            let mut c = ChatComposer::new();
            c.set_text("first command");
            let _ = c.clear_and_submit();
            c.set_text("second command\nwith newline");
            let _ = c.clear_and_submit();
        }

        // Reload — a fresh composer should pick up both entries from disk.
        let c2 = ChatComposer::new();
        assert_eq!(c2.history.len(), 2);
        assert_eq!(c2.history[0], "first command");
        assert_eq!(c2.history[1], "second command\nwith newline");
    }

    #[serial_test::serial]
    #[test]
    #[serial_test::serial]
    fn duplicate_consecutive_submits_are_deduped() {
        let _home = crate::tests::HomeGuard::temp();

        let mut c = ChatComposer::new();
        c.set_text("hi");
        let _ = c.clear_and_submit();
        c.set_text("hi");
        let _ = c.clear_and_submit();
        assert_eq!(c.history.len(), 1);
    }

    #[test]
    fn clear_draft_drops_blobs() {
        let mut c = ChatComposer::new_ephemeral();
        c.handle_paste("1\n2\n3\n4\n5\n");
        assert_eq!(c.pasted_blob_count(), 1);
        c.clear_draft();
        assert_eq!(c.pasted_blob_count(), 0);
        assert!(c.text().is_empty());
    }

    #[test]
    fn idle_placeholder_is_clean_primary_prompt() {
        assert!(IDLE_COMPOSER_PLACEHOLDER.contains("Message"));
        assert!(!IDLE_COMPOSER_PLACEHOLDER.contains("Ctrl+E"));
    }

    #[test]
    fn active_turn_placeholder_is_short_follow_up_prompt() {
        assert!(ACTIVE_TURN_PLACEHOLDER.contains("follow-up"));
        assert!(!ACTIVE_TURN_PLACEHOLDER.contains("Ctrl+C"));
    }
}
