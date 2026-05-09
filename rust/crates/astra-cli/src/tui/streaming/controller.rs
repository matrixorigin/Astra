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

/// Width reserved for the `• ` / `  ` mini-cell prefix that
/// `AgentMessageCell` later prepends during `display_lines`. The raw
/// markdown renderer needs to know about this budget so that wide
/// blocks (tables, rules, code) don't overflow and trigger terminal
/// line-wrap on `│` / `─` characters.
pub(crate) const MINI_CELL_PREFIX_COLS: usize = 2;

impl StreamController {
    pub fn new(width: Option<usize>) -> Self {
        let adj = width.map(adjust_width);
        Self {
            state: StreamState::new(adj),
            raw_source: String::new(),
            rendered_lines: Vec::new(),
            enqueued_len: 0,
            emitted_len: 0,
            width: adj,
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
        let adj = adjust_width(new_width);
        self.width = Some(adj);
        self.state.collector.set_width(Some(adj));

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
        // Tables are rendered atomically on `TagEnd::Table`, but their
        // vertical position shifts as rows arrive: adding a body row
        // pushes the `└┴┘` bottom border one line down. Once we've
        // emitted the "empty" version of the table to scrollback, we
        // can't un-emit it — the row arrives and gets dropped because
        // `enqueued_len` already points past its slot.
        //
        // Defer every line at-or-after the first table boundary until
        // `finalize()`. Non-table content before that still streams
        // token-by-token (fast feedback); tables appear atomically at
        // turn-end (correct structure). Matches Claude Code behavior.
        let safe_end = first_table_line(&self.rendered_lines).unwrap_or(target_len);
        if self.enqueued_len < safe_end {
            let new_lines = self.rendered_lines[self.enqueued_len..safe_end].to_vec();
            self.state.enqueue(new_lines);
            self.enqueued_len = safe_end;
        }
    }
}

/// Find the first rendered line whose content starts (after whitespace)
/// with a box-drawing glyph we use for tables. Returns `None` if none
/// exist in the buffer — the whole output is safe to stream.
fn first_table_line(lines: &[Line<'static>]) -> Option<usize> {
    lines.iter().position(is_table_line)
}

fn is_table_line(line: &Line<'static>) -> bool {
    // Join spans only as far as needed to peek the first non-space char.
    for span in &line.spans {
        for ch in span.content.chars() {
            if ch == ' ' || ch == '\t' {
                continue;
            }
            return matches!(ch, '│' | '┌' | '┐' | '├' | '┤' | '└' | '┘' | '┬' | '┴' | '┼' | '─');
        }
    }
    false
}

#[cfg(test)]
mod table_hold_tests {
    use super::*;

    fn drain_all(sc: &mut StreamController) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let (cell, _idle) = sc.on_commit_tick_batch(20);
            match cell {
                Some(c) => out.push(cell_text(&c)),
                None => break,
            }
        }
        out
    }

    fn cell_text(cell: &Box<dyn ChatCell>) -> String {
        let lines = cell.display_lines(80);
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn regular_text_streams_without_hold() {
        let mut sc = StreamController::new(Some(80));
        sc.push_delta("first paragraph\n\nsecond paragraph\n");
        let emitted = drain_all(&mut sc);
        // Both paragraphs should have reached scrollback via mini-cells.
        let joined = emitted.join("\n");
        assert!(joined.contains("first paragraph"));
        assert!(joined.contains("second paragraph"));
    }

    #[test]
    fn partial_table_is_held_until_finalize() {
        // Feed a complete markdown table across multiple deltas. The
        // borders must only appear in scrollback after finalize — not
        // as partial frames during streaming that would leave a stale
        // bottom border above the real rows.
        let mut sc = StreamController::new(Some(80));
        sc.push_delta("Intro line\n");
        sc.push_delta("| a | b |\n");
        sc.push_delta("|---|---|\n");
        sc.push_delta("| 1 | 2 |\n");

        // Mid-stream drain: intro only, no table glyphs.
        let mid = drain_all(&mut sc).join("\n");
        assert!(mid.contains("Intro line"));
        assert!(
            !mid.contains('┌') && !mid.contains('└'),
            "table borders leaked mid-stream: {mid}"
        );

        // Finalize should release the full table.
        let (final_cell, _source) = sc.finalize();
        let final_text = final_cell
            .map(|c| cell_text(&c))
            .unwrap_or_default();
        assert!(final_text.contains('┌'), "top border missing: {final_text}");
        assert!(final_text.contains('└'), "bottom border missing: {final_text}");
        assert!(final_text.contains("1"), "body row missing: {final_text}");
    }
}

/// Clamp down the raw terminal width by the mini-cell prefix budget,
/// floored at 20 columns so tiny/resizing terminals don't go negative.
fn adjust_width(raw: usize) -> usize {
    raw.saturating_sub(MINI_CELL_PREFIX_COLS).max(20)
}
