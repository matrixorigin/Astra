use std::path::PathBuf;

use ratatui::text::Line;

use super::StreamState;
use crate::tui::chat_cell::ChatCell;
use crate::tui::chat_cell::agent_message_cell::AgentMessageCell;
use crate::tui::markdown::append_markdown;

/// Streams markdown deltas while retaining source for resize reflow.
///
/// Follows Codex's pattern: deltas are buffered until a newline,
/// then rendered lines are queued. Each commit tick drains queued lines
/// into a mini-cell (AgentMessageCell) that gets immediately flushed
/// to terminal scrollback.
pub(crate) struct StreamController {
    state: StreamState,
    raw_source: String,
    rendered_lines: Vec<Line<'static>>,
    enqueued_len: usize,
    emitted_len: usize,
    width: Option<usize>,
    cwd: PathBuf,
    header_emitted: bool,
}

impl StreamController {
    pub fn new(width: Option<usize>) -> Self {
        Self {
            state: StreamState::new(width),
            raw_source: String::new(),
            rendered_lines: Vec::new(),
            enqueued_len: 0,
            emitted_len: 0,
            width,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            header_emitted: false,
        }
    }

    /// Push a text delta. Returns true if new lines were queued.
    pub fn push_delta(&mut self, delta: &str) -> bool {
        self.state.has_seen_delta = true;
        self.state.collector.push_delta(delta);

        if !delta.contains('\n') {
            return false;
        }

        if let Some(committed) = self.state.collector.commit_complete_source() {
            self.raw_source.push_str(&committed);
            self.recompute_and_sync();
            return true;
        }

        false
    }

    /// Drain one batch of queued lines, wrap in a mini-cell.
    /// Returns (cell, is_idle).
    pub fn on_commit_tick(&mut self) -> (Option<Box<dyn ChatCell>>, bool) {
        let batch = self.state.drain_n(1);
        self.emitted_len += batch.len();
        (self.emit(batch), self.state.is_idle())
    }

    /// Drain up to max_lines, wrap in a mini-cell. For catch-up.
    pub fn on_commit_tick_batch(&mut self, max_lines: usize) -> (Option<Box<dyn ChatCell>>, bool) {
        let batch = self.state.drain_n(max_lines);
        self.emitted_len += batch.len();
        (self.emit(batch), self.state.is_idle())
    }

    /// Finalize: commit remaining buffered text, return final mini-cell + raw source.
    pub fn finalize(&mut self) -> (Option<Box<dyn ChatCell>>, Option<String>) {
        // Commit any remaining buffered text
        if let Some(remaining) = self.state.collector.finalize_and_drain_source() {
            if !remaining.is_empty() {
                self.raw_source.push_str(&remaining);
            }
        }

        if self.raw_source.is_empty() {
            return (None, None);
        }

        // Re-render full source to get final lines
        let mut full_rendered = Vec::new();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut full_rendered,
        );

        // Only emit lines not yet emitted
        let remaining_lines = if self.emitted_len < full_rendered.len() {
            full_rendered[self.emitted_len..].to_vec()
        } else {
            Vec::new()
        };

        let source = std::mem::take(&mut self.raw_source);
        let cell = self.emit(remaining_lines);
        (cell, Some(source))
    }

    /// Wrap lines in a mini AgentMessageCell.
    fn emit(&mut self, lines: Vec<Line<'static>>) -> Option<Box<dyn ChatCell>> {
        if lines.is_empty() {
            return None;
        }
        let is_first = !self.header_emitted;
        self.header_emitted = true;
        Some(Box::new(AgentMessageCell::new(lines, is_first)))
    }

    pub fn has_queued(&self) -> bool {
        self.state.queued_len() > 0
    }

    pub fn set_width(&mut self, new_width: usize) {
        self.width = Some(new_width);
        self.state.collector.set_width(Some(new_width));

        if self.raw_source.is_empty() {
            return;
        }

        self.rendered_lines.clear();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut self.rendered_lines,
        );

        let already_emitted = self.emitted_len.min(self.rendered_lines.len());
        self.enqueued_len = already_emitted;
        self.sync_queue();
    }

    fn recompute_and_sync(&mut self) {
        self.rendered_lines.clear();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut self.rendered_lines,
        );
        self.sync_queue();
    }

    fn sync_queue(&mut self) {
        let target_len = self.rendered_lines.len();
        if self.enqueued_len < target_len {
            let new_lines = self.rendered_lines[self.enqueued_len..target_len].to_vec();
            self.state.enqueue(new_lines);
            self.enqueued_len = target_len;
        }
    }
}
