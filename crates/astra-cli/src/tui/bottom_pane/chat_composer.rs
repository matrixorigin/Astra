use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::history_cell::{HistoryCell, user::UserCell};

use super::textarea::{TextArea, TextAreaAction};
use crate::tui::file_writer::TuiFileWriter;

/// Cap on the on-disk history so `~/.astra/history` doesn't grow
/// unbounded. The oldest entries are dropped when the file is
/// rewritten after an append crosses this threshold.
const HISTORY_MAX_ENTRIES: usize = 500;
const HISTORY_SEARCH_MAX_TRANSCRIPT_CELLS: usize = 4096;

/// Duration the `·` prefix glows after the user submits a message.
/// Short enough to feel instantaneous, long enough to be noticed at a
/// glance even when the next frame arrives quickly.
const SUBMIT_FLASH_DURATION: Duration = Duration::from_millis(180);

#[derive(Debug)]
struct HistorySearch {
    query: TextArea,
    entries: Vec<HistorySearchEntry>,
    match_index: Option<usize>,
}

#[derive(Debug)]
enum HistorySearchEntry {
    Session(Arc<dyn HistoryCell>),
    Local(usize),
}

#[derive(Debug)]
pub(crate) struct ChatComposer {
    textarea: TextArea,
    history: Vec<String>,
    history_index: Option<usize>,
    history_search: Option<HistorySearch>,
    draft: Option<String>,
    prompt_prefix: String,
    /// The outer layout can yield the decorative frame to live content on
    /// short terminals without changing the editing model.
    frame_allowed: bool,
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
    file_writer: Option<TuiFileWriter>,
}

/// Multi-line pastes above this threshold are swapped for a placeholder.
/// Single-line pastes are inserted literally regardless of length — users
/// expect URLs and one-liners to appear verbatim.
const PASTE_INLINE_MAX_CHARS: usize = 800;
const PASTE_INLINE_MAX_LINES: usize = 2;
const COMPOSER_PLACEHOLDER: &str = "Message Astra · / for commands";

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
            history_search: None,
            draft: None,
            prompt_prefix: "› ".to_string(),
            frame_allowed: true,
            pasted_blobs: Vec::new(),
            paste_counter: 0,
            last_submit_at: None,
            history_path,
            file_writer: None,
        }
    }

    pub(crate) fn set_file_writer(&mut self, writer: TuiFileWriter) {
        self.file_writer = Some(writer);
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

    /// Command-aware input with presentation-only paste placeholders expanded.
    /// Reading it never consumes pending paste or editing/history state.
    pub(crate) fn expanded_raw_text(&self) -> std::borrow::Cow<'_, str> {
        self.expand_pastes(self.textarea.text())
    }

    pub fn is_empty(&self) -> bool {
        self.history_search.is_none() && self.textarea.is_empty() && !self.paste_burst_active()
    }

    pub(crate) fn paste_burst_active(&self) -> bool {
        self.active_input().paste_burst_active()
    }

    fn active_input(&self) -> &TextArea {
        self.history_search
            .as_ref()
            .map_or(&self.textarea, |search| &search.query)
    }

    fn active_input_mut(&mut self) -> &mut TextArea {
        self.history_search
            .as_mut()
            .map_or(&mut self.textarea, |search| &mut search.query)
    }

    /// True when the user is browsing history (Up/Down navigation).
    /// Callers use this to suppress popup menus that would capture
    /// arrow keys and block further history traversal.
    pub fn is_browsing_history(&self) -> bool {
        self.history_index.is_some() || self.history_search.is_some()
    }

    pub fn is_history_search_active(&self) -> bool {
        self.history_search.is_some()
    }

    // Reverse history search reads `history` directly in this composer.

    pub fn clear_and_submit(&mut self) -> String {
        let raw = self.textarea.text().to_string();
        let expanded = self.expand_pastes(&raw).into_owned();
        // Submission is a semantic boundary, not just a key event. Keep
        // whitespace-only drafts out of the event loop and preserve them so
        // an accidental Enter cannot erase what the user typed.
        if expanded.trim().is_empty() {
            return String::new();
        }
        // Dedup consecutive entries — typing the same command twice in a row
        // shouldn't double up the history list.
        if self.history.last() != Some(&expanded) {
            self.history.push(expanded.clone());
            self.persist_entry(&expanded);
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

    /// Transfer all unsent editing content before clearing its history/paste
    /// state. An unchanged recalled entry remains available in history.
    pub(crate) fn take_recoverable_drafts(&mut self) -> Vec<String> {
        self.cancel_history_search();
        let raw = self.textarea.take_text();
        let visible = self.expand_pastes(&raw).into_owned();
        let mut drafts = Vec::with_capacity(2);
        let unchanged_recall = self
            .history_index
            .is_some_and(|i| self.history[i] == visible);
        if !unchanged_recall && !visible.is_empty() {
            drafts.push(visible);
        }
        if let Some(raw) = self.draft.take() {
            let suspended = self.expand_pastes(&raw).into_owned();
            if !suspended.is_empty() && !drafts.contains(&suspended) {
                drafts.push(suspended);
            }
        }
        self.history_index = None;
        self.pasted_blobs.clear();
        drafts
    }

    pub fn clear_draft(&mut self) {
        self.clear_draft_at(Instant::now());
    }

    pub(super) fn clear_draft_at(&mut self, now: Instant) {
        self.cancel_history_search();
        self.textarea.cancel_pending_paste_at(now);
        self.textarea.clear();
        self.history_index = None;
        self.draft = None;
        self.pasted_blobs.clear();
    }

    /// Handle a bracketed-paste event. Multi-line pastes are collapsed to
    /// a `[Pasted #N · M lines]` placeholder; short pastes go in verbatim.
    /// Returns `true` when a placeholder was inserted.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if self.history_search.is_some() {
            // Keep the query byte-for-byte intact for substring matching. The
            // renderer folds line breaks without changing search semantics.
            self.active_input_mut().insert_str(text);
            self.refresh_history_search();
            return false;
        }
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
    fn expand_pastes<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.pasted_blobs.is_empty() {
            return std::borrow::Cow::Borrowed(text);
        }
        let mut out = text.to_string();
        for (placeholder, blob) in &self.pasted_blobs {
            out = out.replace(placeholder, blob);
        }
        std::borrow::Cow::Owned(out)
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
        if let Some(writer) = self.file_writer.as_ref() {
            if self.history.len() <= HISTORY_MAX_ENTRIES {
                writer.append_line("composer history", path.clone(), encoded);
            } else {
                let keep = &self.history[self.history.len() - HISTORY_MAX_ENTRIES..];
                writer.rewrite_lines(
                    "composer history",
                    path.clone(),
                    keep.iter().map(|line| encode_entry(line)).collect(),
                );
            }
            return;
        }
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
    Some(astra_runtime_env::local_state_root().join("history"))
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
        self.cancel_history_search();
        self.textarea.set_text(text);
    }

    /// Current cursor byte offset within `text()`.
    pub fn cursor_byte(&self) -> usize {
        self.textarea.cursor_byte()
    }

    fn prefix_display_width(&self) -> u16 {
        self.prompt_prefix.width() as u16
    }

    pub(crate) fn consume_guarded_enter_at(&mut self, now: Instant) -> bool {
        let consumed = self.active_input_mut().consume_guarded_enter_at(now);
        if consumed && self.history_search.is_some() {
            self.refresh_history_search();
        }
        consumed
    }

    pub(super) fn consume_guarded_tab_at(&mut self, now: Instant) -> bool {
        let before = self.active_input().text().len();
        let consumed = self.active_input_mut().consume_guarded_tab_at(now);
        if consumed && self.history_search.is_some() && self.active_input().text().len() != before {
            self.refresh_history_search();
        }
        consumed
    }

    pub(super) fn paste_input_pending_at(&mut self, now: Instant) -> bool {
        self.active_input_mut().paste_input_pending_at(now)
    }

    pub(super) fn plain_paste_input_at(
        &mut self,
        now: Instant,
    ) -> Option<super::paste_burst::GuardedInput> {
        self.active_input_mut().plain_paste_input_at(now)
    }

    pub(super) fn release_paste_focus_at(&mut self, now: Instant) -> bool {
        let changed = self.flush_pending_paste_at(now);
        self.active_input_mut().release_paste_focus();
        changed
    }

    pub(super) fn discard_paste_tail_at(
        &mut self,
        now: Instant,
        tail: super::paste_burst::PasteTail,
    ) {
        self.flush_pending_paste_at(now);
        self.active_input_mut().discard_paste_tail(tail);
    }

    pub(super) fn flush_paste_burst_at(&mut self, now: Instant) -> bool {
        let changed = self.active_input_mut().flush_paste_burst_at(now);
        if changed && self.history_search.is_some() {
            self.refresh_history_search();
        }
        changed
    }

    pub(super) fn flush_pending_paste_at(&mut self, now: Instant) -> bool {
        let changed = self.active_input_mut().flush_pending_paste_at(now);
        if changed && self.history_search.is_some() {
            self.refresh_history_search();
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn force_pending_paste_burst_for_test(
        &mut self,
        text: &str,
        now: std::time::Instant,
    ) {
        self.textarea.force_pending_paste_burst_for_test(text, now);
    }

    pub(crate) fn set_frame_allowed(&mut self, allowed: bool) {
        self.frame_allowed = allowed;
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        let framed = self.frame_allowed && width >= 4;
        let horizontal_chrome = if framed { 2 } else { 0 };
        let vertical_chrome = if framed { 2 } else { 1 };
        if self.history_search.is_some() {
            return vertical_chrome + 1;
        }
        let inner_w = width.saturating_sub(horizontal_chrome + self.prefix_display_width());
        self.textarea
            .desired_height(inner_w)
            .saturating_add(vertical_chrome)
            .clamp(if framed { 3 } else { 2 }, if framed { 6 } else { 4 })
    }

    pub(super) fn is_explicit_followup_key(key: KeyEvent) -> bool {
        matches!(key.code, crossterm::event::KeyCode::Char('q' | 'Q'))
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
    }

    fn session_message(cell: &dyn HistoryCell) -> Option<&str> {
        cell.as_any_ref()
            .downcast_ref::<UserCell>()
            .map(UserCell::text)
    }

    fn history_search_entry<'a>(&'a self, entry: &'a HistorySearchEntry) -> Option<&'a str> {
        match entry {
            HistorySearchEntry::Session(cell) => Self::session_message(cell.as_ref()),
            HistorySearchEntry::Local(index) => self.history.get(*index).map(String::as_str),
        }
    }

    fn find_history_match(&self, after: usize) -> Option<usize> {
        let search = self.history_search.as_ref()?;
        let query = search.query.text();
        if query.is_empty() {
            return None;
        }
        search
            .entries
            .iter()
            .enumerate()
            .skip(after)
            .find_map(|(index, entry)| {
                let text = self.history_search_entry(entry)?;
                // A semantic leading slash gains one slash when recalled.
                let escaped_match = matches!(entry, HistorySearchEntry::Session(_))
                    && text.starts_with('/')
                    && query
                        .strip_prefix('/')
                        .is_some_and(|suffix| text.starts_with(suffix));
                (text.contains(query) || escaped_match).then_some(index)
            })
    }

    pub(super) fn start_history_search(&mut self, now: Instant, history: &[Arc<dyn HistoryCell>]) {
        self.textarea.flush_pending_paste_at(now);
        let mut query = TextArea::new();
        self.textarea.swap_paste_burst(&mut query);
        let mut entries = Vec::new();
        let mut session_text = std::collections::HashSet::new();
        let recent_messages = history
            .iter()
            .rev()
            .take(HISTORY_SEARCH_MAX_TRANSCRIPT_CELLS)
            .filter_map(|cell| Some((cell, Self::session_message(cell.as_ref())?)))
            .take(HISTORY_MAX_ENTRIES);
        for (cell, text) in recent_messages {
            let trimmed = text.trim_start();
            // The composer has no literal spelling for shell passthrough or
            // a slash-prefixed message with significant leading whitespace.
            if crate::cli::turn::turn_entry::shell_input_body(text).is_none()
                && (!trimmed.starts_with('/') || trimmed == text)
                && session_text.insert(text)
            {
                entries.push(HistorySearchEntry::Session(Arc::clone(cell)));
            }
        }
        for (index, raw) in self
            .history
            .iter()
            .enumerate()
            .rev()
            .take(HISTORY_MAX_ENTRIES)
        {
            let duplicate = if raw.starts_with('/') {
                raw.strip_prefix('/')
                    .is_some_and(|text| text.starts_with('/') && session_text.contains(text))
            } else {
                session_text.contains(raw.as_str())
            };
            if duplicate {
                continue;
            }
            entries.push(HistorySearchEntry::Local(index));
        }
        self.history_search = Some(HistorySearch {
            query,
            entries,
            match_index: None,
        });
    }

    fn refresh_history_search(&mut self) {
        let match_index = self.find_history_match(0);
        if let Some(search) = &mut self.history_search {
            search.match_index = match_index;
        }
    }

    fn accept_history_search(&mut self) -> Option<String> {
        let search = self.history_search.as_ref()?;
        let entry = search.entries.get(search.match_index?)?;
        let text = self.history_search_entry(entry)?;
        let text = if matches!(entry, HistorySearchEntry::Session(_)) {
            crate::tui::slash_dispatch::conversation_for_composer(text).into_owned()
        } else {
            text.to_owned()
        };
        let mut search = self.history_search.take().expect("matched search");
        self.textarea.swap_paste_burst(&mut search.query);
        Some(text)
    }

    pub(super) fn cancel_history_search(&mut self) -> bool {
        self.cancel_history_search_at(Instant::now())
    }

    pub(super) fn cancel_history_search_at(&mut self, now: Instant) -> bool {
        let Some(mut search) = self.history_search.take() else {
            return false;
        };
        search.query.cancel_pending_paste_at(now);
        search.query.clear();
        self.textarea.swap_paste_burst(&mut search.query);
        true
    }

    pub(super) fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> ComposerAction {
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            if let Some(search) = &mut self.history_search {
                if !search.query.paste_input_pending_at(now) {
                    let after = search.match_index.map_or(0, |index| index + 1);
                    if let Some(next) = self.find_history_match(after) {
                        self.history_search
                            .as_mut()
                            .expect("active search")
                            .match_index = Some(next);
                    }
                }
            }
            return ComposerAction::Consumed;
        }
        if self.history_search.is_some() {
            match key.code {
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    self.active_input_mut().handle_key_at(key, now);
                    self.refresh_history_search();
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    if !self.consume_guarded_enter_at(now)
                        && let Some(text) = self.accept_history_search()
                    {
                        return ComposerAction::Recall(text);
                    }
                }
                KeyCode::Esc => {
                    self.cancel_history_search_at(now);
                }
                KeyCode::Tab if key.modifiers.is_empty() => {
                    if self.active_input_mut().insert_key_char_at('\t', now) {
                        self.refresh_history_search();
                    }
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if self.active_input_mut().insert_key_char_at(c, now) {
                        self.refresh_history_search();
                    }
                }
                KeyCode::Char('y')
                    if key.modifiers == KeyModifiers::CONTROL
                        && self.active_input_mut().paste_input_pending_at(now) =>
                {
                    // Yank normally releases the paste guard. Keep that
                    // guard with bytes still arriving in this input gesture.
                }
                code if code == KeyCode::Backspace
                    || matches!(code, KeyCode::Char('u' | 'w' | 'k' | 'y'))
                        && key.modifiers == KeyModifiers::CONTROL =>
                {
                    self.active_input_mut().handle_key_at(key, now);
                    self.refresh_history_search();
                }
                _ => {}
            }
            return ComposerAction::Consumed;
        }
        if Self::is_explicit_followup_key(key) {
            // Never clear a draft while part of a non-bracketed paste is
            // still buffered. A due burst can join this submission; an
            // ongoing burst stays in the composer for a later keypress.
            self.flush_paste_burst_at(now);
            return if self.textarea.paste_burst_active() || self.textarea.text().trim().is_empty() {
                ComposerAction::Consumed
            } else {
                ComposerAction::QueueFollowUp
            };
        }
        if key.code == crossterm::event::KeyCode::Char('e')
            && key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
            && !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            self.textarea.flush_pending_paste_at(now);
            return ComposerAction::OpenExternalEditor;
        }
        match self.textarea.handle_key_at(key, now) {
            TextAreaAction::Submit => {
                if self.textarea.text().trim().is_empty() {
                    ComposerAction::Consumed
                } else {
                    ComposerAction::Submit
                }
            }
            TextAreaAction::Cancel => {
                if !self.is_empty() {
                    self.clear_draft_at(now);
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
                self.draft = Some(self.textarea.take_text());
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

    fn history_search_line(
        &self,
        width: u16,
        panel_bg: Color,
        include_preview: bool,
    ) -> Option<(Line<'static>, usize)> {
        let search = self.history_search.as_ref()?;
        let theme = crate::tui::theme::current();
        let label = if search.match_index.is_some() || search.query.is_empty() {
            "(reverse-i-search)"
        } else {
            "(failed reverse-i-search)"
        };
        let width = usize::from(width);
        let query_text = search.query.text();
        let start = query_text
            .grapheme_indices(true)
            .rev()
            .nth(width)
            .map_or(0, |(index, grapheme)| index + grapheme.len());
        let display_query = format!(
            "{}{}",
            if start > 0 { "…" } else { "" },
            history_search_display(&query_text[start..])
        );
        let has_preview = search.match_index.is_some();
        let long_opening = format!("{label}`");
        let opening = if width
            >= long_opening.width()
                + display_query.width().min(8)
                + 3
                + usize::from(has_preview) * 4
        {
            long_opening
        } else if search.match_index.is_some() || search.query.is_empty() {
            "? ".to_owned()
        } else {
            "! ".to_owned()
        };
        let separator = if width >= opening.width() + 3 {
            "': "
        } else {
            ""
        };
        let available = width.saturating_sub(opening.width() + separator.width());
        let preview_reserve = if has_preview {
            (available / 2).min(8)
        } else {
            0
        };
        let display_query =
            truncate_start(&display_query, available.saturating_sub(preview_reserve));
        let preview_width = available.saturating_sub(display_query.width());
        let preview = search
            .match_index
            .filter(|_| include_preview)
            .and_then(|index| search.entries.get(index))
            .and_then(|entry| self.history_search_entry(entry))
            .map(|entry| {
                // Only the visible prefix is needed; a history entry can be large.
                let prefix: String = entry
                    .graphemes(true)
                    .take(preview_width.saturating_add(1))
                    .collect();
                truncate_end(&history_search_display(&prefix), preview_width)
            })
            .unwrap_or_default();
        let cursor = opening.width() + display_query.width();
        let dim = Style::default().fg(theme.dim).bg(panel_bg);
        let query = Style::default().fg(theme.accent).bg(panel_bg).bold();
        let result = Style::default().fg(theme.fg).bg(panel_bg);
        Some((
            Line::from(vec![
                Span::styled(opening, dim),
                Span::styled(display_query, query),
                Span::styled(separator, dim),
                Span::styled(preview, result),
            ]),
            cursor,
        ))
    }

    fn render_history_search(&self, area: Rect, buf: &mut Buffer, panel_bg: Color) {
        if let Some((line, _)) = self.history_search_line(area.width, panel_bg, true) {
            Widget::render(line, Rect::new(area.x, area.y, area.width, 1), buf);
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, queueing: bool) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let theme = crate::tui::theme::current();
        let panel = crate::tui::style::composer_surface_style();
        let panel_bg = panel.bg.unwrap_or(Color::Reset);
        let active_color = if queueing || self.is_flashing() {
            theme.accent
        } else {
            theme.accent_dim()
        };
        fill_area(buf, area, panel);
        if self.frame_allowed && composer_has_frame(area) {
            render_composer_frame(buf, area, Style::default().fg(active_color).bg(panel_bg));
        }
        let content_area = composer_content_area(area, self.frame_allowed);
        if self.history_search.is_some() {
            self.render_history_search(content_area, buf, panel_bg);
            return;
        }

        // Pending input feedback keeps its existing semantic glyph and adds a
        // matching frame accent. Neither changes Enter routing or queue state.
        let prefix_style = Style::default().fg(active_color).bg(panel_bg);
        let prefix_str: &str = if queueing {
            "↳ "
        } else {
            &self.prompt_prefix
        };
        let prefix = Span::styled(prefix_str, prefix_style);
        // Measure the same prefix that is emitted so the cursor remains anchored
        // to the text edge for every queueing state.
        let prefix_width = (prefix_str.width() as u16).min(content_area.width).max(1);
        let prefix_area = Rect::new(
            content_area.x,
            content_area.y,
            prefix_width.min(content_area.width),
            1,
        );
        Widget::render(Line::from(prefix), prefix_area, buf);
        // Continuations stay aligned under the input for explicit newlines,
        // wrapping and paste. The guide is never part of the submitted text.
        let continuation = Span::styled(
            crate::tui::glyphs::current().live_gutter.to_string(),
            Style::default().fg(theme.dim).bg(panel_bg),
        );
        let visible_rows = self
            .textarea
            .desired_height(content_area.width.saturating_sub(prefix_width))
            .min(content_area.height);
        for y in content_area.y.saturating_add(1)..content_area.y + visible_rows {
            Widget::render(
                Line::from(continuation.clone()),
                Rect::new(content_area.x, y, prefix_area.width, 1),
                buf,
            );
        }

        let text_area = Rect::new(
            content_area.x + prefix_width.min(content_area.width),
            content_area.y,
            content_area.width.saturating_sub(prefix_width),
            content_area.height.max(1),
        );

        if self.textarea.is_empty() {
            let placeholder = Span::styled(
                truncate_end(COMPOSER_PLACEHOLDER, text_area.width as usize),
                Style::default().fg(theme.dim).bg(panel_bg),
            );
            Widget::render(
                Line::from(placeholder),
                Rect::new(text_area.x, text_area.y, text_area.width, 1),
                buf,
            );
        } else {
            self.textarea.render(text_area, buf);
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let content_area = composer_content_area(area, self.frame_allowed);
        if let Some((_, cursor)) = self.history_search_line(content_area.width, Color::Reset, false)
        {
            let x = content_area
                .x
                .saturating_add(cursor as u16)
                .min(content_area.right().saturating_sub(1));
            return Some((x, content_area.y));
        }
        let prefix_width = self.prefix_display_width();
        let text_area = Rect::new(
            content_area.x + prefix_width.min(content_area.width),
            content_area.y,
            content_area.width.saturating_sub(prefix_width),
            content_area.height.max(1),
        );
        self.textarea.cursor_position(text_area)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    Recall(String),
    Submit,
    QueueFollowUp,
    OpenExternalEditor,
    Interrupt,
    Quit,
    Consumed,
    Unhandled,
}

fn history_search_display(text: &str) -> String {
    let mut display = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                display.push_str(" ↩ ");
            }
            '\n' => display.push_str(" ↩ "),
            ch if ch.is_control() => display.push(' '),
            ch => display.push(ch),
        }
    }
    display
}

fn composer_has_frame(area: Rect) -> bool {
    area.width >= 4 && area.height >= 3
}

/// The only source of composer insets. Rendering and cursor placement must use
/// the same geometry, especially when a small terminal suppresses the frame.
fn composer_content_area(area: Rect, frame_allowed: bool) -> Rect {
    if frame_allowed && composer_has_frame(area) {
        Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
    } else {
        area
    }
}

fn render_composer_frame(buf: &mut Buffer, area: Rect, style: Style) {
    let horizontal = "─".repeat(area.width as usize - 2);
    buf.set_string(area.x, area.y, format!("╭{horizontal}╮"), style);
    for y in area.y + 1..area.y + area.height - 1 {
        buf.set_string(area.x, y, "│", style);
        buf.set_string(area.x + area.width - 1, y, "│", style);
    }
    buf.set_string(
        area.x,
        area.y + area.height - 1,
        format!("╰{horizontal}╯"),
        style,
    );
}

fn fill_area(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.y + area.height {
        buf.set_string(area.x, y, " ".repeat(area.width as usize), style);
    }
}

fn truncate_end(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
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
    for grapheme in text.graphemes(true) {
        let w = grapheme.width();
        if used + w > keep {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out.push('…');
    out
}

fn truncate_start(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_owned();
    }
    if max_width == 1 {
        return text
            .graphemes(true)
            .next_back()
            .filter(|grapheme| grapheme.width() == 1)
            .unwrap_or("…")
            .to_owned();
    }
    let mut start = text.len();
    let mut used = 0;
    for (index, grapheme) in text.grapheme_indices(true).rev() {
        let width = grapheme.width();
        if used + width >= max_width {
            break;
        }
        used += width;
        start = index;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn multiline_input_uses_a_visual_continuation_guide_without_editing_the_draft() {
        let mut composer = ChatComposer::new_ephemeral();
        composer.set_text("你好ab\n  z");
        let area = Rect::new(0, 0, 10, 4);
        let mut buffer = Buffer::empty(area);
        composer.render(area, &mut buffer, false);
        assert_eq!(buffer[(0, 0)].symbol(), "╭");
        assert_eq!(buffer[(1, 1)].symbol(), "›");
        assert_eq!(
            buffer[(1, 2)].symbol(),
            crate::tui::glyphs::current().live_gutter.to_string()
        );
        assert_eq!(buffer[(5, 2)].symbol(), "z");
        assert_eq!(composer.cursor_position(area), Some((6, 2)));
        assert_eq!(composer.text(), "你好ab\n  z");
    }

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
    fn whitespace_submit_is_consumed_without_clearing_the_draft() {
        let mut c = ChatComposer::new_ephemeral();
        c.set_text("  \n\t");

        assert_eq!(
            c.handle_key_at(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                Instant::now()
            ),
            ComposerAction::Consumed
        );
        assert_eq!(c.text(), "  \n\t");
        assert!(!c.is_flashing());
    }

    #[test]
    fn explicit_followup_flushes_due_paste_and_keeps_an_ongoing_burst() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        let mut composer = ChatComposer::new_ephemeral();
        composer.set_text("prefix ");
        composer.force_pending_paste_burst_for_test("pasted suffix", Instant::now());
        assert_eq!(
            composer.handle_key_at(key, Instant::now()),
            ComposerAction::QueueFollowUp
        );
        assert_eq!(composer.clear_and_submit(), "prefix pasted suffix");

        composer.set_text("another prefix ");
        // Keep the simulated burst fresh for this keypress, without sleeps.
        composer.force_pending_paste_burst_for_test(
            "still buffered",
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(
            composer.handle_key_at(key, Instant::now()),
            ComposerAction::Consumed
        );
        assert_eq!(composer.text(), "another prefix ");
        assert!(composer.textarea.paste_burst_active());
    }

    #[tokio::test]
    async fn submitted_history_uses_ordered_writer_and_flushes_exact_entries() {
        let dir = crate::tests::test_temp_dir();
        let path = dir.path().join("history");
        let (writer, runtime, mut errors) = crate::tui::file_writer::spawn();
        let mut composer = ChatComposer::build(Vec::new(), Some(path.clone()));
        composer.set_file_writer(writer.clone());

        composer.set_text("first");
        assert_eq!(composer.clear_and_submit(), "first");
        composer.set_text("second\nline");
        assert_eq!(composer.clear_and_submit(), "second\nline");
        writer.flush().await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "first\nsecond\\nline\n"
        );
        assert!(errors.try_recv().is_err());
        runtime.shutdown().await.unwrap();
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
    fn idle_placeholder_exposes_command_discovery_without_key_clutter() {
        assert!(COMPOSER_PLACEHOLDER.contains("Message Astra"));
        assert!(COMPOSER_PLACEHOLDER.contains("/ for commands"));
        assert!(!COMPOSER_PLACEHOLDER.contains("Ctrl+E"));
    }

    #[test]
    fn composer_grows_around_multiline_input() {
        let mut composer = ChatComposer::new_ephemeral();
        assert_eq!(composer.desired_height(80), 3);

        composer.set_text("first\nsecond\nthird");
        assert_eq!(composer.desired_height(80), 5);
        assert_eq!(composer.desired_height(0), 0);
    }

    #[test]
    fn framed_composer_shares_render_and_cursor_geometry() {
        let composer = ChatComposer::new_ephemeral();
        let area = Rect::new(2, 3, 42, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 48, 8));
        composer.render(area, &mut buffer, false);

        assert_eq!(buffer[(2, 3)].symbol(), "╭");
        assert_eq!(buffer[(43, 3)].symbol(), "╮");
        assert_eq!(buffer[(2, 5)].symbol(), "╰");
        assert_eq!(buffer[(43, 5)].symbol(), "╯");
        assert_eq!(composer.cursor_position(area), Some((5, 4)));
    }

    #[test]
    fn compact_composer_drops_the_frame_before_the_editing_row() {
        let composer = ChatComposer::new_ephemeral();
        let area = Rect::new(0, 0, 3, 2);
        let mut buffer = Buffer::empty(area);
        composer.render(area, &mut buffer, false);

        assert_eq!(buffer[(0, 0)].symbol(), "›");
        assert!(
            composer
                .cursor_position(area)
                .is_some_and(|(x, y)| x < area.width && y < area.height)
        );
        assert!(
            !buffer
                .content
                .iter()
                .any(|cell| matches!(cell.symbol(), "╭" | "╮" | "╰" | "╯"))
        );
    }

    #[test]
    fn buffered_search_keys_preserve_selection_until_paste_flush() {
        let mut composer = ChatComposer::new_ephemeral();
        for entry in ["ab older", "abcx\tnewer"] {
            composer.set_text(entry);
            composer.clear_and_submit();
        }

        let now = Instant::now();
        composer.start_history_search(now, &[]);
        composer.handle_paste("ab");
        composer.handle_key_at(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            now,
        );
        assert_eq!(
            composer.history_search.as_ref().unwrap().match_index,
            Some(1)
        );

        composer
            .active_input_mut()
            .force_pending_paste_burst_for_test("c", now);
        for code in [KeyCode::Char('x'), KeyCode::Tab] {
            composer.handle_key_at(KeyEvent::new(code, KeyModifiers::NONE), now);
            assert_eq!(composer.active_input().text(), "ab");
            assert_eq!(
                composer.history_search.as_ref().unwrap().match_index,
                Some(1)
            );
        }

        assert!(composer.flush_pending_paste_at(now));
        assert_eq!(composer.active_input().text(), "abcx\t");
        assert_eq!(
            composer.handle_key_at(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                now + Duration::from_secs(1)
            ),
            ComposerAction::Recall("abcx\tnewer".into())
        );
    }
}
