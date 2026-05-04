pub(crate) mod controller;

use std::collections::VecDeque;
use std::time::Instant;

use ratatui::text::Line;

use super::markdown_stream::MarkdownStreamCollector;

pub(crate) struct StreamState {
    pub collector: MarkdownStreamCollector,
    queued_lines: VecDeque<QueuedLine>,
    pub has_seen_delta: bool,
    /// How many emitted lines have been flushed to scrollback already.
    pub scrollback_flushed: usize,
}

struct QueuedLine {
    line: Line<'static>,
    #[allow(dead_code)]
    enqueued_at: Instant,
}

impl StreamState {
    pub fn new(width: Option<usize>) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width),
            queued_lines: VecDeque::new(),
            has_seen_delta: false,
            scrollback_flushed: 0,
        }
    }

    pub fn step(&mut self) -> Option<Line<'static>> {
        self.queued_lines.pop_front().map(|ql| ql.line)
    }

    pub fn drain_n(&mut self, max: usize) -> Vec<Line<'static>> {
        let n = max.min(self.queued_lines.len());
        self.queued_lines.drain(..n).map(|ql| ql.line).collect()
    }

    pub fn is_idle(&self) -> bool {
        self.queued_lines.is_empty()
    }

    pub fn queued_len(&self) -> usize {
        self.queued_lines.len()
    }

    pub fn enqueue(&mut self, lines: Vec<Line<'static>>) {
        let now = Instant::now();
        for line in lines {
            self.queued_lines.push_back(QueuedLine {
                line,
                enqueued_at: now,
            });
        }
    }
}
