//! Non-blocking event loop for Cursor-style CLI UX.
//!
//! Provides:
//! - Non-blocking input handling (user can type during generation)
//! - Fixed terminal layout (status bar + input box at bottom)
//! - Input queue for pending commands
//! - Cancellation support via Ctrl+C

#![allow(dead_code)] // Module is being incrementally integrated

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};

/// Token for cooperative cancellation of running tasks.
#[derive(Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check if cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Reset for reuse.
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }
}

/// Input buffer with editing support.
pub struct InputBuffer {
    /// Current input text (may be multi-line).
    text: String,
    /// Cursor position in the text.
    cursor_pos: usize,
    /// Command history.
    history: Vec<String>,
    /// Current history index (None = editing new input).
    history_index: Option<usize>,
    /// Saved input when browsing history.
    saved_input: Option<String>,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_index: None,
            saved_input: None,
        }
    }

    /// Load history from a file (one command per line).
    pub fn load_history(&mut self, path: &std::path::Path) {
        if let Ok(contents) = std::fs::read_to_string(path) {
            self.history = contents.lines().map(|s| s.to_string()).collect();
        }
    }

    /// Append a command to history.
    pub fn add_history(&mut self, cmd: &str) {
        if !cmd.is_empty() && self.history.last().map(|s| s.as_str()) != Some(cmd) {
            self.history.push(cmd.to_string());
        }
    }

    /// Get current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Get cursor position.
    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    /// Clear input and return the text.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor_pos = 0;
        self.history_index = None;
        self.saved_input = None;
        text
    }

    /// Clear input.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
        self.history_index = None;
        self.saved_input = None;
    }

    /// Insert a character at cursor.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    /// Insert a string at cursor.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor_pos, s);
        self.cursor_pos += s.len();
    }

    /// Delete character before cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            // Find the start of the previous character.
            let prev_pos = self.text[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.remove(prev_pos);
            self.cursor_pos = prev_pos;
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.text.remove(self.cursor_pos);
        }
    }

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.text[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor_pos < self.text.len() {
            self.cursor_pos += self.text[self.cursor_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
        }
    }

    /// Move cursor to start.
    pub fn move_home(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to end.
    pub fn move_end(&mut self) {
        self.cursor_pos = self.text.len();
    }

    /// Navigate to previous history entry.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                // Save current input and go to last history entry.
                self.saved_input = Some(self.text.clone());
                self.history_index = Some(self.history.len() - 1);
                self.text = self.history[self.history.len() - 1].clone();
                self.cursor_pos = self.text.len();
            }
            Some(idx) if idx > 0 => {
                self.history_index = Some(idx - 1);
                self.text = self.history[idx - 1].clone();
                self.cursor_pos = self.text.len();
            }
            _ => {}
        }
    }

    /// Navigate to next history entry.
    pub fn history_next(&mut self) {
        match self.history_index {
            Some(idx) if idx < self.history.len() - 1 => {
                self.history_index = Some(idx + 1);
                self.text = self.history[idx + 1].clone();
                self.cursor_pos = self.text.len();
            }
            Some(_) => {
                // Restore saved input.
                self.history_index = None;
                if let Some(saved) = self.saved_input.take() {
                    self.text = saved;
                    self.cursor_pos = self.text.len();
                }
            }
            _ => {}
        }
    }

    /// Delete word before cursor (Ctrl+W).
    pub fn delete_word(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        // Find start of previous word.
        let before = &self.text[..self.cursor_pos];
        let trimmed = before.trim_end();
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        self.text = format!(
            "{}{}",
            &self.text[..word_start],
            &self.text[self.cursor_pos..]
        );
        self.cursor_pos = word_start;
    }

    /// Clear line (Ctrl+U).
    pub fn clear_line(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Queue for pending commands (typed while task is running).
pub struct InputQueue {
    queue: VecDeque<String>,
}

impl InputQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Add a command to the queue.
    pub fn push(&mut self, cmd: String) {
        self.queue.push_back(cmd);
    }

    /// Take the next command.
    pub fn pop(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Number of pending commands.
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

impl Default for InputQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Status bar information.
pub struct StatusBar {
    /// Current model name.
    pub model: String,
    /// Tokens used: (prompt, completion).
    pub tokens: (u64, u64),
    /// Session ID (short form).
    pub session_id: Option<String>,
    /// Whether currently thinking/generating.
    pub is_thinking: bool,
    /// Current mode (normal, plan, paused).
    pub mode: StatusMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusMode {
    Normal,
    Plan,
    Paused,
    PlanOnly,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            model: "auto".to_string(),
            tokens: (0, 0),
            session_id: None,
            is_thinking: false,
            mode: StatusMode::Normal,
        }
    }

    /// Render status bar to a string.
    pub fn render(&self, width: usize) -> String {
        let model_part = format!("⬢ {}", self.model);
        let tokens_part = format!(
            "↓{}k ↑{}k",
            format_tokens(self.tokens.0),
            format_tokens(self.tokens.1)
        );
        let session_part = self
            .session_id
            .as_ref()
            .map(|s| format!("session: {}", &s[..s.len().min(8)]))
            .unwrap_or_default();
        let mode_part = match self.mode {
            StatusMode::Normal => "",
            StatusMode::Plan => " [plan]",
            StatusMode::Paused => " [paused]",
            StatusMode::PlanOnly => " [plan·]",
        };
        let thinking_part = if self.is_thinking { " ●" } else { "" };

        let content = format!(
            "{}{} | {} | {}{}",
            model_part, mode_part, tokens_part, session_part, thinking_part
        );

        // Pad to terminal width.
        if content.len() >= width {
            content[..width].to_string()
        } else {
            format!("{:width$}", content, width = width)
        }
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}", n as f64 / 1000.0)
    } else {
        format!("0.{}", n / 100)
    }
}

/// Fixed terminal layout with output area, status bar, and input box.
pub struct TerminalLayout {
    /// Terminal width.
    width: u16,
    /// Terminal height.
    height: u16,
    /// Lines reserved for status + input.
    reserved_bottom: u16,
    /// Current output scroll position.
    scroll_offset: usize,
}

impl TerminalLayout {
    pub fn new() -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        Ok(Self {
            width,
            height,
            reserved_bottom: 3, // 1 status + 1 separator + 1 input
            scroll_offset: 0,
        })
    }

    /// Update terminal size.
    pub fn refresh_size(&mut self) -> io::Result<()> {
        let (width, height) = terminal::size()?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Available height for output.
    pub fn output_height(&self) -> u16 {
        self.height.saturating_sub(self.reserved_bottom)
    }

    /// Terminal width.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Move cursor to status bar line.
    pub fn move_to_status(&self) -> io::Result<()> {
        let status_line = self.height.saturating_sub(self.reserved_bottom);
        execute!(io::stdout(), cursor::MoveTo(0, status_line))?;
        Ok(())
    }

    /// Move cursor to input line.
    pub fn move_to_input(&self) -> io::Result<()> {
        let input_line = self.height.saturating_sub(1);
        execute!(io::stdout(), cursor::MoveTo(0, input_line))?;
        Ok(())
    }

    /// Render status bar at its fixed position.
    pub fn render_status(&self, status: &StatusBar) -> io::Result<()> {
        self.move_to_status()?;
        execute!(io::stdout(), terminal::Clear(ClearType::CurrentLine))?;

        // Dim background for status bar.
        let line = status.render(self.width as usize);
        print!("\x1b[48;5;236m{}\x1b[0m", line);
        io::stdout().flush()?;
        Ok(())
    }

    /// Render input box at its fixed position.
    pub fn render_input(&self, prompt: &str, buffer: &InputBuffer) -> io::Result<()> {
        self.move_to_input()?;
        execute!(io::stdout(), terminal::Clear(ClearType::CurrentLine))?;

        let text = buffer.text();
        let cursor_pos = buffer.cursor_pos();

        // Calculate visible portion of input.
        let prompt_len = prompt.chars().count();
        let available = (self.width as usize).saturating_sub(prompt_len + 1);

        // Simple case: fits on one line.
        if text.len() <= available {
            print!("{}{}", prompt, text);
            io::stdout().flush()?;

            // Position cursor.
            let cursor_col = prompt_len + text[..cursor_pos].chars().count();
            execute!(
                io::stdout(),
                cursor::MoveTo(cursor_col as u16, self.height - 1)
            )?;
        } else {
            // Scroll view to keep cursor visible.
            let cursor_chars = text[..cursor_pos].chars().count();
            let start = if cursor_chars >= available {
                cursor_chars - available + 1
            } else {
                0
            };

            let visible: String = text.chars().skip(start).take(available).collect();
            print!("{}{}", prompt, visible);
            io::stdout().flush()?;

            let cursor_col = prompt_len + cursor_chars - start;
            execute!(
                io::stdout(),
                cursor::MoveTo(cursor_col as u16, self.height - 1)
            )?;
        }

        Ok(())
    }

    /// Clear and redraw the entire layout.
    pub fn redraw_all(
        &self,
        status: &StatusBar,
        prompt: &str,
        buffer: &InputBuffer,
    ) -> io::Result<()> {
        // Clear bottom reserved area.
        self.move_to_status()?;
        execute!(io::stdout(), terminal::Clear(ClearType::FromCursorDown))?;

        self.render_status(status)?;
        self.render_input(prompt, buffer)?;

        Ok(())
    }
}

impl Default for TerminalLayout {
    fn default() -> Self {
        Self::new().unwrap_or(Self {
            width: 80,
            height: 24,
            reserved_bottom: 3,
            scroll_offset: 0,
        })
    }
}

/// Poll for input events without blocking.
///
/// Returns `Some(event)` if an event is available within the timeout,
/// or `None` if no event is ready.
pub fn poll_event(timeout: Duration) -> io::Result<Option<Event>> {
    if event::poll(timeout)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}

/// Handle a key event, updating the input buffer.
///
/// Returns:
/// - `Some(text)` if Enter was pressed (submit command)
/// - `None` if the event was handled but no submission
pub fn handle_key_event(
    key: KeyEvent,
    buffer: &mut InputBuffer,
    cancel_token: &CancellationToken,
) -> Option<String> {
    match (key.code, key.modifiers) {
        // Submit on Enter.
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let text = buffer.take();
            Some(text)
        }
        // Newline on Shift+Enter (for multi-line input).
        (KeyCode::Enter, KeyModifiers::SHIFT) => {
            buffer.insert_char('\n');
            None
        }
        // Cancel on Ctrl+C.
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            cancel_token.cancel();
            buffer.clear();
            None
        }
        // Clear line on Ctrl+U.
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            buffer.clear_line();
            None
        }
        // Delete word on Ctrl+W.
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            buffer.delete_word();
            None
        }
        // Exit on Ctrl+D (if buffer is empty).
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            if buffer.text().is_empty() {
                Some("/exit".to_string())
            } else {
                buffer.delete();
                None
            }
        }
        // Navigation.
        (KeyCode::Left, _) => {
            buffer.move_left();
            None
        }
        (KeyCode::Right, _) => {
            buffer.move_right();
            None
        }
        (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            buffer.move_home();
            None
        }
        (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            buffer.move_end();
            None
        }
        (KeyCode::Up, _) => {
            buffer.history_prev();
            None
        }
        (KeyCode::Down, _) => {
            buffer.history_next();
            None
        }
        // Editing.
        (KeyCode::Backspace, _) => {
            buffer.backspace();
            None
        }
        (KeyCode::Delete, _) => {
            buffer.delete();
            None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            buffer.insert_char(c);
            None
        }
        _ => None,
    }
}

/// Event loop runner that handles input while tasks are running.
///
/// This is the core of the Cursor-style UX. It:
/// 1. Accepts user input at any time (even during generation)
/// 2. Queues commands while a task is running
/// 3. Handles Ctrl+C to cancel the running task
/// 4. Updates the status bar and input display continuously
pub struct EventLoopRunner {
    /// Input buffer for the current line.
    pub input: InputBuffer,
    /// Queue for commands typed while busy.
    pub queue: InputQueue,
    /// Terminal layout manager.
    pub layout: TerminalLayout,
    /// Status bar state.
    pub status: StatusBar,
    /// Cancellation token for the current task.
    pub cancel_token: CancellationToken,
    /// Whether currently executing a task.
    is_busy: bool,
    /// Current prompt string.
    prompt: String,
}

impl EventLoopRunner {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            input: InputBuffer::new(),
            queue: InputQueue::new(),
            layout: TerminalLayout::new()?,
            status: StatusBar::new(),
            cancel_token: CancellationToken::new(),
            is_busy: false,
            prompt: "❯ ".to_string(),
        })
    }

    /// Set the prompt string.
    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
    }

    /// Set busy state (task is running).
    pub fn set_busy(&mut self, busy: bool) {
        self.is_busy = busy;
        self.status.is_thinking = busy;
        if !busy {
            // Reset cancel token for next task.
            self.cancel_token.reset();
        }
    }

    /// Check if a task is currently running.
    pub fn is_busy(&self) -> bool {
        self.is_busy
    }

    /// Update model in status bar.
    pub fn set_model(&mut self, model: &str) {
        self.status.model = model.to_string();
    }

    /// Update token counts in status bar.
    pub fn set_tokens(&mut self, prompt_tokens: u64, completion_tokens: u64) {
        self.status.tokens = (prompt_tokens, completion_tokens);
    }

    /// Update session ID in status bar.
    pub fn set_session(&mut self, session_id: Option<&str>) {
        self.status.session_id = session_id.map(|s| s.to_string());
    }

    /// Update mode in status bar.
    pub fn set_mode(&mut self, mode: StatusMode) {
        self.status.mode = mode;
    }

    /// Render the fixed layout (status bar + input).
    pub fn render(&self) -> io::Result<()> {
        self.layout
            .redraw_all(&self.status, &self.prompt, &self.input)
    }

    /// Process a single input event.
    ///
    /// Returns `Some(command)` if a command was submitted.
    pub fn process_event(&mut self, event: Event) -> Option<String> {
        match event {
            Event::Key(key) => {
                if let Some(cmd) = handle_key_event(key, &mut self.input, &self.cancel_token) {
                    if self.is_busy {
                        // Queue command for later.
                        if !cmd.is_empty() {
                            self.queue.push(cmd);
                        }
                        None
                    } else {
                        // Execute immediately.
                        self.input.add_history(&cmd);
                        Some(cmd)
                    }
                } else {
                    // Redraw input after keystroke.
                    let _ = self.layout.render_input(&self.prompt, &self.input);
                    None
                }
            }
            Event::Resize(width, height) => {
                self.layout.width = width;
                self.layout.height = height;
                let _ = self.render();
                None
            }
            _ => None,
        }
    }

    /// Poll for next command, handling input events.
    ///
    /// This is non-blocking - returns `None` if no command is ready.
    pub fn poll_command(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        // Check queued commands first (if not busy).
        if !self.is_busy && !self.queue.is_empty() {
            return Ok(self.queue.pop());
        }

        // Poll for input events.
        if let Some(event) = poll_event(timeout)? {
            return Ok(self.process_event(event));
        }

        Ok(None)
    }

    /// Enter raw mode for event-driven input.
    pub fn enter_raw_mode() -> io::Result<()> {
        terminal::enable_raw_mode()
    }

    /// Exit raw mode.
    pub fn exit_raw_mode() -> io::Result<()> {
        terminal::disable_raw_mode()
    }
}

impl Default for EventLoopRunner {
    fn default() -> Self {
        Self::new().expect("Failed to create EventLoopRunner")
    }
}

impl Drop for EventLoopRunner {
    fn drop(&mut self) {
        // Ensure we exit raw mode.
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_buffer_basic() {
        let mut buf = InputBuffer::new();
        buf.insert_char('h');
        buf.insert_char('i');
        assert_eq!(buf.text(), "hi");
        assert_eq!(buf.cursor_pos(), 2);

        buf.backspace();
        assert_eq!(buf.text(), "h");
        assert_eq!(buf.cursor_pos(), 1);
    }

    #[test]
    fn test_input_buffer_navigation() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello");
        assert_eq!(buf.cursor_pos(), 5);

        buf.move_left();
        assert_eq!(buf.cursor_pos(), 4);

        buf.move_home();
        assert_eq!(buf.cursor_pos(), 0);

        buf.move_end();
        assert_eq!(buf.cursor_pos(), 5);
    }

    #[test]
    fn test_input_buffer_history() {
        let mut buf = InputBuffer::new();
        buf.add_history("first");
        buf.add_history("second");
        buf.add_history("third");

        buf.insert_str("current");
        buf.history_prev();
        assert_eq!(buf.text(), "third");

        buf.history_prev();
        assert_eq!(buf.text(), "second");

        buf.history_next();
        assert_eq!(buf.text(), "third");

        buf.history_next();
        assert_eq!(buf.text(), "current");
    }

    #[test]
    fn test_cancellation_token() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());

        token.cancel();
        assert!(token.is_cancelled());

        token.reset();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_input_queue() {
        let mut queue = InputQueue::new();
        assert!(queue.is_empty());

        queue.push("cmd1".to_string());
        queue.push("cmd2".to_string());
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.pop(), Some("cmd1".to_string()));
        assert_eq!(queue.pop(), Some("cmd2".to_string()));
        assert!(queue.is_empty());
    }

    #[test]
    fn test_status_bar_render() {
        let mut status = StatusBar::new();
        status.model = "gpt-4".to_string();
        status.tokens = (1500, 500);
        status.session_id = Some("abc123def456".to_string());

        let rendered = status.render(60);
        assert!(rendered.contains("gpt-4"));
        assert!(rendered.contains("1.5k"));
        assert!(rendered.contains("abc123de"));
    }

    #[test]
    fn test_delete_word() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello world");
        buf.delete_word();
        assert_eq!(buf.text(), "hello ");

        buf.delete_word();
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn test_unicode_navigation() {
        let mut buf = InputBuffer::new();
        buf.insert_str("你好");
        assert_eq!(buf.cursor_pos(), 6); // 2 chars * 3 bytes each

        buf.move_left();
        assert_eq!(buf.cursor_pos(), 3);

        buf.move_left();
        assert_eq!(buf.cursor_pos(), 0);

        buf.move_right();
        assert_eq!(buf.cursor_pos(), 3);
    }
}
