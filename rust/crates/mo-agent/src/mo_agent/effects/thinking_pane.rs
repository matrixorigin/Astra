//! Reasoning preview pane.
//!
//! Shows reasoning/thinking content in a viewport that grows until a cap,
//! then folds away old lines with a "hidden lines above" header.

use super::super::terminal_region::TerminalRegion;
use crossterm::style::Stylize;

/// Max **content** rows for `thinking_delta` / `reasoning_delta` (`0` = spinner only).
/// While under this cap the pane **grows downward** (no blank padding). Past the cap, the top
/// folds away and a `... (N lines hidden above)` header appears above the tail.
pub fn thinking_viewport_rows() -> usize {
    std::env::var("MO_AGENT_THINKING_VIEWPORT_LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(|n: usize| n.min(24))
        .unwrap_or(6)
}

/// Thinking preview pane using TerminalRegion for flicker-free stdout rendering.
///
/// Unlike the old stderr-based implementation, this uses stdout + TerminalRegion
/// so it shares the same cursor coordinate space as StreamingMarkdown. This
/// prevents the cursor desync issues that caused duplicate lines.
pub struct ThinkingPreviewPane {
    body_rows: usize,
    width: usize,
    buffer: String,
    /// Region for diff-based updates (stdout).
    region: TerminalRegion,
}

impl ThinkingPreviewPane {
    /// Create a new pane with the given maximum body rows and width.
    pub fn new(body_rows: usize, width: usize) -> Self {
        Self {
            body_rows: body_rows.max(1),
            width: width.max(20),
            buffer: String::new(),
            region: TerminalRegion::new(),
        }
    }

    /// Push a chunk of reasoning content and redraw.
    pub fn push_chunk(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        const CAP: usize = 48 * 1024;
        if self.buffer.len() > CAP {
            let overflow = self.buffer.len() - CAP / 2;
            // Find the next valid UTF-8 char boundary after the overflow point
            // to avoid splitting multi-byte characters.
            let drain_end = self
                .buffer
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| i >= overflow)
                .unwrap_or(overflow);
            self.buffer.drain(..drain_end);
        }
        self.redraw();
    }

    fn build_frame(&self) -> (String, Vec<String>) {
        let w = self.width.saturating_sub(6).max(12);
        let visual = buffer_to_visual_lines(&self.buffer, w);
        let cap = self.body_rows;
        if visual.is_empty() {
            return (String::new(), Vec::new());
        }
        let hidden = visual.len().saturating_sub(cap);
        let body: Vec<String> = if hidden == 0 {
            visual
        } else {
            visual[visual.len() - cap..].to_vec()
        };
        let header = if hidden > 0 {
            format!("... ({hidden} lines hidden above)")
        } else {
            String::new()
        };
        (header, body)
    }

    /// Redraw using TerminalRegion (stdout) for flicker-free diff-based updates.
    fn redraw(&mut self) {
        let (header, body) = self.build_frame();
        if header.is_empty() && body.is_empty() {
            self.region.update(Vec::new());
            return;
        }
        let mut lines = Vec::with_capacity(body.len() + 1);
        if !header.is_empty() {
            lines.push(format!("  {}", header.dim()));
        }
        for line in body {
            if line.is_empty() {
                lines.push(String::new());
            } else {
                lines.push(format!("  {} {}", "◇".dim(), line.dim()));
            }
        }
        self.region.update(lines);
    }

    /// Clear the pane content and terminal region.
    pub fn clear(&mut self) {
        self.region.clear();
        self.buffer.clear();
    }

    /// Return the number of lines currently displayed.
    #[allow(dead_code)]
    pub fn height(&self) -> usize {
        self.region.height()
    }
}

// ═══════════════════════════════════════════════════════════════ Helpers ══

/// Split one logical line into fixed-width visual rows (UTF-8 safe).
fn wrap_line_to_width(line: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![line.to_string()];
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let end = (i + w).min(chars.len());
        out.push(chars[i..end].iter().collect());
        i = end;
    }
    out
}

/// Expand buffer into visual rows (preserve newlines; wrap long lines).
fn buffer_to_visual_lines(buffer: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    if buffer.is_empty() {
        return out;
    }
    // Normalize line endings: \r\n -> \n, standalone \r -> \n
    let normalized = buffer.replace("\r\n", "\n").replace('\r', "\n");
    for raw_line in normalized.split('\n') {
        let line = raw_line.trim_end_matches([' ', '\t']);
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        out.extend(wrap_line_to_width(line, w));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_line_to_width() {
        assert_eq!(wrap_line_to_width("hello", 3), vec!["hel", "lo"]);
        assert_eq!(wrap_line_to_width("abc", 5), vec!["abc"]);
        assert_eq!(wrap_line_to_width("", 5), vec![""]);
    }

    #[test]
    fn test_buffer_to_visual_lines() {
        let lines = buffer_to_visual_lines("hello\nworld", 80);
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn test_crlf_normalization() {
        let lines = buffer_to_visual_lines("a\r\nb\rc", 80);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_utf8_safe_truncation() {
        let mut p = ThinkingPreviewPane::new(4, 80);
        // Push Chinese text
        p.push_chunk("你好世界！这是一个测试。");
        // Should not panic and buffer should contain the text
        assert!(p.buffer.contains("你好"));
    }

    #[test]
    fn thinking_preview_pane_no_top_padding_before_cap() {
        let mut p = ThinkingPreviewPane::new(4, 80);
        p.buffer = "line1\nline2".into();
        let (h, b) = p.build_frame();
        assert!(h.is_empty(), "no hidden header while under cap");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], "line1");
        assert_eq!(b[1], "line2");
    }

    #[test]
    fn thinking_preview_pane_tail_and_header_after_cap() {
        let mut p = ThinkingPreviewPane::new(2, 80);
        p.buffer = "a\nb\nc\nd".into();
        let (h, b) = p.build_frame();
        assert_eq!(h, "... (2 lines hidden above)");
        assert_eq!(b, vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn thinking_pane_buffer_truncation_is_utf8_safe() {
        let mut pane = ThinkingPreviewPane::new(3, 80);
        // Create a string with multi-byte UTF-8 characters
        let chinese = "中文测试内容";
        // Repeat enough times to exceed the 48KB cap
        let repeated = chinese.repeat(10000); // ~60KB of UTF-8 content
        pane.push_chunk(&repeated);
        // After truncation, the buffer should still be valid UTF-8
        // (this would panic if we split a multi-byte char)
        assert!(pane.buffer.is_char_boundary(0));
        assert!(pane.buffer.is_char_boundary(pane.buffer.len()));
        // Verify we can iterate chars without panicking
        let _: usize = pane.buffer.chars().count();
    }

    #[test]
    fn buffer_to_visual_lines_handles_crlf() {
        // Windows-style \r\n should become single newline
        let s = "line1\r\nline2\r\nline3";
        let lines = buffer_to_visual_lines(s, 100);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[1], "line2");
        assert_eq!(lines[2], "line3");
    }

    #[test]
    fn buffer_to_visual_lines_handles_mixed_line_endings() {
        // Mix of \r\n, \n, and \r
        let s = "a\r\nb\nc\rd";
        let lines = buffer_to_visual_lines(s, 100);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn wrap_line_to_width_is_utf8_safe() {
        let rows = wrap_line_to_width("在/tmp下面", 2);
        assert!(rows.iter().all(|r| r.chars().count() <= 2));
        assert_eq!(rows.join(""), "在/tmp下面");
    }

    #[test]
    fn hidden_line_count_matches_cursor_style_overflow() {
        let visual = buffer_to_visual_lines("a\nb\nc\nd\ne", 80);
        assert_eq!(visual.len(), 5);
        let body_cap = 3usize;
        assert_eq!(visual.len().saturating_sub(body_cap), 2);
        assert_eq!(visual[visual.len() - body_cap..], ["c", "d", "e"]);
    }
}
