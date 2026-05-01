use std::path::PathBuf;

use ratatui::text::Line;

use super::StreamState;
use crate::tui::chat_cell::assistant_cell::AssistantChatCell;
use crate::tui::chat_cell::ChatCell;
use crate::tui::markdown::append_markdown;
use crate::tui::render::line_utils::line_to_static;

/// Streams markdown deltas while retaining source for resize reflow.
///
/// Follows Codex's newline-gated pattern: deltas are buffered until a newline
/// is seen, then the completed source is rendered and queued for display.
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

    /// Finalize the stream, returning a ChatCell with all content + the raw source.
    pub fn finalize(mut self) -> (Option<Box<dyn ChatCell>>, Option<String>) {
        if let Some(remaining) = self.state.collector.finalize_and_drain_source() {
            self.raw_source.push_str(&remaining);
        }

        if self.raw_source.is_empty() {
            return (None, None);
        }

        let source = self.raw_source.clone();
        let cell = AssistantChatCell::from_source(
            source.clone(),
            self.width.unwrap_or(80) as u16,
        );
        (Some(Box::new(cell)), Some(source))
    }

    /// Drain queued lines, returning them if available.
    pub fn tick(&mut self) -> Option<Vec<Line<'static>>> {
        let batch = self.state.drain_n(5);
        if batch.is_empty() {
            None
        } else {
            self.emitted_len += batch.len();
            Some(batch)
        }
    }

    /// Flush any pending (uncommitted) text for immediate display.
    /// Called periodically so short responses without newlines still render.
    pub fn flush_pending(&mut self) {
        if let Some(remaining) = self.state.collector.finalize_and_drain_source() {
            if !remaining.is_empty() {
                self.raw_source.push_str(&remaining);
                self.recompute_and_sync();
            }
        }
        // Re-create the collector so future deltas continue to buffer
        self.state.collector = crate::tui::markdown_stream::MarkdownStreamCollector::new(self.width);
    }

    /// Returns all lines emitted so far (for building a transient cell).
    pub fn emitted_lines(&self) -> &[Line<'static>] {
        &self.rendered_lines[..self.emitted_len.min(self.rendered_lines.len())]
    }

    pub fn queued_len(&self) -> usize {
        self.state.queued_len()
    }

    pub fn is_idle(&self) -> bool {
        self.state.is_idle()
    }

    pub fn set_width(&mut self, new_width: usize) {
        self.width = Some(new_width);
        self.state.collector.set_width(Some(new_width));

        if self.raw_source.is_empty() {
            return;
        }

        // Recompute rendered lines from source at new width
        self.rendered_lines.clear();
        append_markdown(
            &self.raw_source,
            self.width,
            Some(self.cwd.as_path()),
            &mut self.rendered_lines,
        );

        // Re-sync queue: only queue lines beyond what was already emitted
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
        if self.enqueued_len < self.rendered_lines.len() {
            let new_lines: Vec<Line<'static>> = self.rendered_lines[self.enqueued_len..]
                .iter()
                .map(|l| line_to_static(l))
                .collect();
            self.state.enqueue(new_lines);
            self.enqueued_len = self.rendered_lines.len();
        }
    }
}
