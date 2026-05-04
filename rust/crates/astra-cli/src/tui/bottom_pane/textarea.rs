use std::cell::RefCell;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

// ─── TextArea ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct TextArea {
    text: String,
    cursor_pos: usize,
    preferred_col: Option<usize>,
    wrap_cache: RefCell<Option<WrapCache>>,
    kill_buffer: String,
    scroll: u16,
}

#[derive(Debug)]
struct WrapCache {
    width: u16,
    lines: Vec<Range<usize>>,
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_pos: 0,
            preferred_col: None,
            wrap_cache: RefCell::new(None),
            kill_buffer: String::new(),
            scroll: 0,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor_pos = self.text.len();
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 1;
        }
        let lines = self.wrapped_lines(width);
        (lines.len() as u16).clamp(1, 6)
    }

    // ─── Input handling ─────────────────────────────────────────────────

    pub fn handle_key(&mut self, key: KeyEvent) -> TextAreaAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Enter if shift || alt => {
                self.insert_char('\n');
                TextAreaAction::Changed
            }
            KeyCode::Enter => TextAreaAction::Submit,

            // Emacs keybindings
            KeyCode::Char('c') if ctrl => TextAreaAction::Cancel,
            KeyCode::Char('d') if ctrl => {
                if self.is_empty() {
                    TextAreaAction::Quit
                } else {
                    self.delete_forward();
                    TextAreaAction::Changed
                }
            }
            KeyCode::Char('a') if ctrl => {
                self.move_to_line_start();
                TextAreaAction::Changed
            }
            KeyCode::Char('e') if ctrl => {
                self.move_to_line_end();
                TextAreaAction::Changed
            }
            KeyCode::Char('k') if ctrl => {
                self.kill_to_end_of_line();
                TextAreaAction::Changed
            }
            KeyCode::Char('y') if ctrl => {
                self.yank();
                TextAreaAction::Changed
            }
            KeyCode::Char('w') if ctrl => {
                self.delete_backward_word();
                TextAreaAction::Changed
            }

            // Word movement
            KeyCode::Left if ctrl || alt => {
                self.move_word_left();
                TextAreaAction::Changed
            }
            KeyCode::Right if ctrl || alt => {
                self.move_word_right();
                TextAreaAction::Changed
            }
            KeyCode::Char('b') if alt => {
                self.move_word_left();
                TextAreaAction::Changed
            }
            KeyCode::Char('f') if alt => {
                self.move_word_right();
                TextAreaAction::Changed
            }

            // Basic editing
            KeyCode::Char(c) => {
                self.insert_char(c);
                TextAreaAction::Changed
            }
            KeyCode::Backspace => {
                self.delete_backward();
                TextAreaAction::Changed
            }
            KeyCode::Delete => {
                self.delete_forward();
                TextAreaAction::Changed
            }

            // Cursor movement
            KeyCode::Left => {
                self.move_left();
                TextAreaAction::Changed
            }
            KeyCode::Right => {
                self.move_right();
                TextAreaAction::Changed
            }
            KeyCode::Up => TextAreaAction::HistoryPrev,
            KeyCode::Down => TextAreaAction::HistoryNext,
            KeyCode::Home => {
                self.move_to_line_start();
                TextAreaAction::Changed
            }
            KeyCode::End => {
                self.move_to_line_end();
                TextAreaAction::Changed
            }
            _ => TextAreaAction::Unhandled,
        }
    }

    // ─── Text mutation ──────────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor_pos, s);
        self.cursor_pos += s.len();
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    fn delete_backward(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let prev = self.prev_grapheme_boundary(self.cursor_pos);
        self.text.drain(prev..self.cursor_pos);
        self.cursor_pos = prev;
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    fn delete_forward(&mut self) {
        if self.cursor_pos >= self.text.len() {
            return;
        }
        let next = self.next_grapheme_boundary(self.cursor_pos);
        self.text.drain(self.cursor_pos..next);
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    fn kill_to_end_of_line(&mut self) {
        let line_end = self.text[self.cursor_pos..]
            .find('\n')
            .map(|i| self.cursor_pos + i)
            .unwrap_or(self.text.len());

        if line_end == self.cursor_pos && self.cursor_pos < self.text.len() {
            // At EOL: kill the newline
            self.kill_buffer = self.text[self.cursor_pos..self.cursor_pos + 1].to_string();
            self.text.drain(self.cursor_pos..self.cursor_pos + 1);
        } else {
            self.kill_buffer = self.text[self.cursor_pos..line_end].to_string();
            self.text.drain(self.cursor_pos..line_end);
        }
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let kb = self.kill_buffer.clone();
        self.insert_str(&kb);
    }

    fn delete_backward_word(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let target = self.beginning_of_previous_word();
        self.kill_buffer = self.text[target..self.cursor_pos].to_string();
        self.text.drain(target..self.cursor_pos);
        self.cursor_pos = target;
        self.preferred_col = None;
        self.invalidate_wrap();
    }

    // ─── Cursor movement ────────────────────────────────────────────────

    fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.prev_grapheme_boundary(self.cursor_pos);
            self.preferred_col = None;
        }
    }

    fn move_right(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.cursor_pos = self.next_grapheme_boundary(self.cursor_pos);
            self.preferred_col = None;
        }
    }

    fn move_to_line_start(&mut self) {
        let line_start = self.text[..self.cursor_pos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor_pos = line_start;
        self.preferred_col = None;
    }

    fn move_to_line_end(&mut self) {
        let line_end = self.text[self.cursor_pos..]
            .find('\n')
            .map(|i| self.cursor_pos + i)
            .unwrap_or(self.text.len());
        self.cursor_pos = line_end;
        self.preferred_col = None;
    }

    fn move_word_left(&mut self) {
        self.cursor_pos = self.beginning_of_previous_word();
        self.preferred_col = None;
    }

    fn move_word_right(&mut self) {
        self.cursor_pos = self.end_of_next_word();
        self.preferred_col = None;
    }

    // ─── Word boundary helpers ──────────────────────────────────────────

    fn beginning_of_previous_word(&self) -> usize {
        if self.cursor_pos == 0 {
            return 0;
        }
        let before = &self.text[..self.cursor_pos];
        // Skip trailing whitespace
        let trimmed_end = before
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| {
                i + self.text[i..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        if trimmed_end == 0 {
            return 0;
        }

        let search_slice = &self.text[..trimmed_end];
        // Find start of word (scan backward for whitespace or separator boundary)
        let mut pos = trimmed_end;
        let last_char = search_slice.chars().last().unwrap_or(' ');
        let is_sep = WORD_SEPARATORS.contains(last_char);

        for (i, c) in search_slice.char_indices().rev() {
            if c.is_whitespace() {
                pos = i + c.len_utf8();
                break;
            }
            if is_sep != WORD_SEPARATORS.contains(c) {
                pos = i + c.len_utf8();
                break;
            }
            pos = i;
        }
        pos
    }

    fn end_of_next_word(&self) -> usize {
        if self.cursor_pos >= self.text.len() {
            return self.text.len();
        }
        let after = &self.text[self.cursor_pos..];
        // Skip leading whitespace
        let skip_ws = after
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after.len());

        let search_start = self.cursor_pos + skip_ws;
        if search_start >= self.text.len() {
            return self.text.len();
        }

        let search_slice = &self.text[search_start..];
        let first_char = search_slice.chars().next().unwrap_or(' ');
        let is_sep = WORD_SEPARATORS.contains(first_char);

        for (i, c) in search_slice.char_indices().skip(1) {
            if c.is_whitespace() {
                return search_start + i;
            }
            if is_sep != WORD_SEPARATORS.contains(c) {
                return search_start + i;
            }
        }
        self.text.len()
    }

    // ─── Grapheme boundary helpers ──────────────────────────────────────

    fn prev_grapheme_boundary(&self, pos: usize) -> usize {
        let before = &self.text[..pos];
        before
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_grapheme_boundary(&self, pos: usize) -> usize {
        let after = &self.text[pos..];
        after
            .grapheme_indices(true)
            .nth(1)
            .map(|(i, _)| pos + i)
            .unwrap_or(self.text.len())
    }

    // ─── Wrap cache ─────────────────────────────────────────────────────

    fn invalidate_wrap(&self) {
        *self.wrap_cache.borrow_mut() = None;
    }

    fn wrapped_lines(&self, width: u16) -> Vec<Range<usize>> {
        {
            let cache = self.wrap_cache.borrow();
            if let Some(ref c) = *cache {
                if c.width == width {
                    return c.lines.clone();
                }
            }
        }

        let lines = compute_wrap_ranges(&self.text, width);

        *self.wrap_cache.borrow_mut() = Some(WrapCache {
            width,
            lines: lines.clone(),
        });

        lines
    }

    fn wrapped_line_index(&self, lines: &[Range<usize>], byte_pos: usize) -> usize {
        for (i, range) in lines.iter().enumerate() {
            if byte_pos < range.end || i + 1 == lines.len() {
                return i;
            }
        }
        0
    }

    // ─── Rendering ──────────────────────────────────────────────────────

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let lines = self.wrapped_lines(area.width);
        let max_rows = area.height as usize;

        let cursor_line = self.wrapped_line_index(&lines, self.cursor_pos);
        let scroll = if cursor_line >= self.scroll as usize + max_rows {
            cursor_line - max_rows + 1
        } else if (self.scroll as usize) > cursor_line {
            cursor_line
        } else {
            self.scroll as usize
        };

        for (row, line_idx) in (scroll..lines.len()).enumerate() {
            if row >= max_rows {
                break;
            }
            let range = &lines[line_idx];
            let end = range.end.min(self.text.len());
            let slice = &self.text[range.start..end];
            let y = area.y + row as u16;
            buf.set_string(area.x, y, slice, Style::default());
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if area.width == 0 {
            return None;
        }

        let lines = self.wrapped_lines(area.width);
        let max_rows = area.height as usize;

        let cursor_line = self.wrapped_line_index(&lines, self.cursor_pos);
        let scroll = if cursor_line >= self.scroll as usize + max_rows {
            cursor_line - max_rows + 1
        } else if (self.scroll as usize) > cursor_line {
            cursor_line
        } else {
            self.scroll as usize
        };

        let visible_row = cursor_line.checked_sub(scroll)?;
        if visible_row >= max_rows {
            return None;
        }

        let line_range = &lines[cursor_line];
        let line_start = line_range.start;
        let pos_in_line = self
            .cursor_pos
            .min(self.text.len())
            .saturating_sub(line_start);
        let slice_end = (line_start + pos_in_line).min(self.text.len());
        let display_col = UnicodeWidthStr::width(&self.text[line_start..slice_end]);

        Some((area.x + display_col as u16, area.y + visible_row as u16))
    }
}

// ─── Wrap computation ───────────────────────────────────────────────────────

#[allow(clippy::single_range_in_vec_init)]
fn compute_wrap_ranges(text: &str, width: u16) -> Vec<Range<usize>> {
    if text.is_empty() {
        return vec![0..0];
    }

    let w = width as usize;
    if w == 0 {
        return vec![0..text.len()];
    }

    let mut ranges = Vec::new();

    for (line_start, logical_line) in split_logical_lines(text) {
        if logical_line.is_empty() {
            ranges.push(line_start..line_start);
            continue;
        }

        let mut pos = 0;
        let mut current_w = 0;

        for (gi, grapheme) in logical_line.grapheme_indices(true) {
            let gw = UnicodeWidthStr::width(grapheme);
            if current_w + gw > w && current_w > 0 {
                ranges.push((line_start + pos)..(line_start + gi));
                pos = gi;
                current_w = 0;
            }
            current_w += gw;
        }
        ranges.push((line_start + pos)..(line_start + logical_line.len()));
    }

    if ranges.is_empty() {
        ranges.push(0..0);
    }

    ranges
}

fn split_logical_lines(text: &str) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            result.push((start, &text[start..i]));
            start = i + 1;
        }
    }
    result.push((start, &text[start..]));
    result
}

// ─── Actions ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextAreaAction {
    Changed,
    Submit,
    Cancel,
    Quit,
    HistoryPrev,
    HistoryNext,
    Consumed,
    Unhandled,
}
