/// Collects markdown stream source at newline boundaries.
///
/// Buffers token deltas and exposes commit boundaries at each newline.
/// Only complete lines (up to the last `\n`) are committed for rendering,
/// avoiding half-parsed code blocks or URLs.
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_len: usize,
    width: Option<usize>,
}

impl MarkdownStreamCollector {
    pub fn new(width: Option<usize>) -> Self {
        Self {
            buffer: String::new(),
            committed_len: 0,
            width,
        }
    }

    pub fn set_width(&mut self, width: Option<usize>) {
        self.width = width;
    }

    pub fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    /// Commit completed source up to the last newline.
    /// Returns the newly committed markdown, or None if no newline found.
    pub fn commit_complete_source(&mut self) -> Option<String> {
        let search_region = &self.buffer[self.committed_len..];
        let last_newline = search_region.rfind('\n')?;
        let boundary = self.committed_len + last_newline + 1;

        let committed = self.buffer[self.committed_len..boundary].to_string();
        self.committed_len = boundary;
        Some(committed)
    }

    /// Finalize: return any remaining uncommitted source.
    pub fn finalize_and_drain_source(&mut self) -> Option<String> {
        if self.committed_len >= self.buffer.len() {
            return None;
        }
        let remaining = self.buffer[self.committed_len..].to_string();
        self.committed_len = self.buffer.len();
        if remaining.is_empty() {
            None
        } else {
            Some(remaining)
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.committed_len = 0;
    }
}

impl std::fmt::Debug for MarkdownStreamCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkdownStreamCollector")
            .field("buffer_len", &self.buffer.len())
            .field("committed_len", &self.committed_len)
            .finish()
    }
}
