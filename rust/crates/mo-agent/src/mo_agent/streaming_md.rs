//! Incremental markdown streaming renderer.
//!
//! Inspired by claudecode's `StreamingMarkdown` component: split accumulated
//! text at the last complete top-level block boundary.  Everything before the
//! boundary is "stable" — rendered once and never touched again.  Only the
//! trailing "unstable" block is cleared and re-rendered on each delta.
//!
//! This eliminates the flash-clear-rerender cycle that made the old pipeline
//! feel janky.

use std::io::{self, Write};
use termimad::crossterm::style::Color;
use termimad::{FmtText, MadSkin};

/// Incremental markdown renderer that streams formatted output.
pub(super) struct StreamingMarkdown {
    /// Full accumulated text so far.
    full_text: String,
    /// Byte offset into `full_text` up to which we have already printed
    /// stable (finalized) markdown blocks.
    stable_end: usize,
    /// Number of terminal lines occupied by the last unstable render.
    /// Used to clear the unstable region before re-rendering.
    unstable_lines: usize,
    /// Whether the last render left the cursor mid-line (no trailing newline).
    has_partial: bool,
    /// Terminal width for rendering.
    term_width: usize,
    /// Total terminal lines written (stable + current unstable).
    pub lines_written: usize,
}

impl StreamingMarkdown {
    pub(super) fn new(term_width: usize) -> Self {
        Self {
            full_text: String::new(),
            stable_end: 0,
            unstable_lines: 0,
            has_partial: false,
            term_width: term_width.max(20),
            lines_written: 0,
        }
    }

    /// Append a text delta and incrementally render.
    pub(super) fn push(&mut self, delta: &str) {
        self.full_text.push_str(delta);

        // Strip XML-style thinking/reflect tags that leaked into text output.
        // Must adjust stable_end if stripping happened before it.
        let old_len = self.full_text.len();
        strip_xml_tags_inplace(&mut self.full_text);
        if self.full_text.len() < old_len {
            self.stable_end = self.stable_end.min(self.full_text.len());
        }

        // Don't render until we have at least one complete block boundary.
        if self.stable_end == 0 && find_last_block_boundary(&self.full_text) == 0 {
            return;
        }

        // Throttle: only re-render on newlines or large deltas.
        if !delta.contains('\n') && delta.len() < 20 {
            return;
        }

        self.render_incremental();
    }

    /// Force a render of any pending content (called on newlines and finish).
    fn render_incremental(&mut self) {
        let new_stable_end = find_last_block_boundary(&self.full_text);

        // If stable region grew, print the newly-stable portion.
        if new_stable_end > self.stable_end {
            self.clear_unstable();

            let new_stable = &self.full_text[self.stable_end..new_stable_end];
            let rendered = render_md(new_stable, self.term_width);
            print!("{rendered}");
            let _ = io::stdout().flush();
            self.lines_written += count_lines(&rendered, self.term_width);
            self.has_partial = has_partial_line(&rendered);
            self.stable_end = new_stable_end;
            self.unstable_lines = 0;
        } else {
            self.clear_unstable();
        }

        // Render the unstable suffix.
        let unstable = &self.full_text[self.stable_end..];
        if !unstable.is_empty() {
            let rendered = render_md(unstable, self.term_width);
            print!("{rendered}");
            let _ = io::stdout().flush();
            let lines = count_lines(&rendered, self.term_width);
            self.unstable_lines = lines;
            self.lines_written += lines;
            self.has_partial = has_partial_line(&rendered);
        }
    }

    /// Finalize: render any buffered content and finalize the unstable region.
    pub(super) fn finish(&mut self) {
        // Render any content that was buffered (no newline yet).
        self.render_incremental();
    }

    /// Account for a line written by something else (e.g. tool_request notice
    /// or thinking duration) that was interleaved into our output region.
    /// NOTE: These are typically on stderr and do NOT affect stdout cursor.
    /// We track them only so `clear_all` can account for them.
    pub(super) fn track_eprintln(&mut self) {
        self.lines_written += 1;
        // Do NOT add to unstable_lines — stderr doesn't move stdout cursor.
    }

    /// Clear ALL rendered output (stable + unstable).
    /// Used for intermediate agentic turns where tool_calls are pending —
    /// the draft text must not leak into the terminal.
    pub(super) fn clear_all(&mut self) {
        let total = self.lines_written;
        if total > 0 || self.has_partial {
            use crossterm::{cursor, execute, terminal};
            if total > 0 {
                execute!(
                    io::stdout(),
                    cursor::MoveUp(total as u16),
                )
                .ok();
            }
            execute!(
                io::stdout(),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        }
        self.lines_written = 0;
        self.unstable_lines = 0;
        self.has_partial = false;
        self.stable_end = 0;
        self.full_text.clear();
    }

    fn clear_unstable(&mut self) {
        let total = self.unstable_lines;
        if total > 0 || self.has_partial {
            use crossterm::{cursor, execute, terminal};
            if total > 0 {
                execute!(
                    io::stdout(),
                    cursor::MoveUp(total as u16),
                )
                .ok();
            }
            execute!(
                io::stdout(),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
            self.lines_written = self.lines_written.saturating_sub(total);
            self.unstable_lines = 0;
            self.has_partial = false;
        }
    }
}

/// Strip LLM thinking/reflect XML tags from text in-place.
/// Handles both complete tags `<reflect>...</reflect>` and partial opening
/// tags `<reflect>...` (closing tag hasn't arrived yet).
fn strip_xml_tags_inplace(text: &mut String) {
    const TAGS: &[&str] = &["reflect", "thinking", "inner_monologue"];
    let mut changed = false;
    for tag in TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        // Strip complete tags
        while let Some(start) = text.find(&open) {
            if let Some(end) = text[start..].find(&close) {
                let remove_end = start + end + close.len();
                // Also strip trailing newline if present
                let remove_end = if text.as_bytes().get(remove_end) == Some(&b'\n') {
                    remove_end + 1
                } else {
                    remove_end
                };
                text.drain(start..remove_end);
                changed = true;
            } else {
                // Partial tag — opening found but no closing yet.
                // Truncate from the opening tag (it will be re-added by next delta).
                text.truncate(start);
                changed = true;
                break;
            }
        }
    }
    if changed {
        // Collapse any resulting double-newlines into single
        while text.contains("\n\n\n") {
            *text = text.replace("\n\n\n", "\n\n");
        }
    }
}

fn make_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    skin.bold.set_fg(Color::Cyan);
    skin.italic.set_fg(Color::Yellow);
    skin.inline_code.set_fg(Color::Green);
    skin
}

fn render_md(text: &str, width: usize) -> String {
    let skin = make_skin();
    let fmt = FmtText::from(&skin, text, Some(width));
    format!("{fmt}")
}

/// Count how many terminal lines a rendered string occupies.
/// Returns the number of lines the cursor moved down — i.e. the number of
/// `\n` characters.  A non-empty string without a trailing newline still
/// has content on the current line, but the cursor hasn't moved to a new line.
fn count_lines(rendered: &str, _term_width: usize) -> usize {
    if rendered.is_empty() {
        return 0;
    }
    rendered.chars().filter(|&c| c == '\n').count()
}

/// Whether the rendered string left the cursor mid-line (no trailing newline).
fn has_partial_line(rendered: &str) -> bool {
    !rendered.is_empty() && !rendered.ends_with('\n')
}

/// Find the byte offset of the last "stable" block boundary.
///
/// A block boundary is a position after a `\n\n` where the next content
/// starts a new top-level markdown block.  We scan backwards for `\n\n`
/// and verify the character after it looks like a block start.
///
/// Returns 0 if no boundary found (everything is unstable).
fn find_last_block_boundary(text: &str) -> usize {
    // We need at least a double-newline to have a boundary.
    if text.len() < 3 {
        return 0;
    }

    // Scan backwards for "\n\n" positions.
    let bytes = text.as_bytes();
    let mut i = bytes.len().saturating_sub(2);
    while i > 0 {
        if bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            // Found a double-newline at position i.
            // The stable region ends at i+2 (after the double-newline).
            let boundary = i + 2;
            // Check if what follows looks like a block start.
            if boundary < bytes.len() && is_block_start(&text[boundary..]) {
                return boundary;
            }
        }
        i -= 1;
    }
    0
}

/// Check if text starts with a markdown block-level element.
fn is_block_start(text: &str) -> bool {
    let t = text.trim_start_matches(' ');
    if t.is_empty() {
        return false;
    }
    // Heading
    if t.starts_with('#') {
        return true;
    }
    // Code fence
    if t.starts_with("```") || t.starts_with("~~~") {
        return true;
    }
    // Unordered list
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    // Ordered list (digit followed by . or ))
    if let Some(first) = t.as_bytes().first() {
        if first.is_ascii_digit() {
            if let Some(rest) = t.strip_prefix(|c: char| c.is_ascii_digit()) {
                if rest.starts_with(". ") || rest.starts_with(") ") {
                    return true;
                }
            }
        }
    }
    // Blockquote
    if t.starts_with('>') {
        return true;
    }
    // Horizontal rule
    if t.starts_with("---") || t.starts_with("***") || t.starts_with("___") {
        return true;
    }
    // Table (pipe at start)
    if t.starts_with('|') {
        return true;
    }
    // Paragraph (any other text) — this IS a block start
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_boundary_in_single_block() {
        assert_eq!(find_last_block_boundary("hello world"), 0);
    }

    #[test]
    fn boundary_at_paragraph_break() {
        let text = "first paragraph\n\nsecond paragraph";
        let b = find_last_block_boundary(text);
        assert_eq!(b, 17); // after "\n\n"
        assert_eq!(&text[b..], "second paragraph");
    }

    #[test]
    fn boundary_at_heading() {
        let text = "some text\n\n## Heading\n\nmore text";
        let b = find_last_block_boundary(text);
        // Should find the last boundary (before "more text")
        assert_eq!(&text[b..], "more text");
    }

    #[test]
    fn boundary_at_code_fence() {
        let text = "intro\n\n```rust\nfn main() {}\n```";
        let b = find_last_block_boundary(text);
        assert_eq!(&text[b..], "```rust\nfn main() {}\n```");
    }

    #[test]
    fn count_lines_basic() {
        assert_eq!(count_lines("hello\nworld\n", 80), 2);
        assert_eq!(count_lines("no newline", 80), 0);
        assert_eq!(count_lines("", 80), 0);
    }

    #[test]
    fn render_md_produces_output() {
        let out = render_md("**bold** text", 80);
        assert!(!out.is_empty());
    }

    #[test]
    fn streaming_md_incremental() {
        let mut sm = StreamingMarkdown::new(80);
        sm.full_text.push_str("hello ");
        let b = find_last_block_boundary(&sm.full_text);
        assert_eq!(b, 0);

        sm.full_text.push_str("world\n\nnew paragraph");
        let b = find_last_block_boundary(&sm.full_text);
        assert!(b > 0);
        assert_eq!(&sm.full_text[b..], "new paragraph");
    }

    #[test]
    fn strip_reflect_tags() {
        let mut s = "before\n<reflect>thinking here</reflect>\nafter".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "before\nafter");
    }

    #[test]
    fn strip_partial_reflect_tag() {
        let mut s = "text before <reflect>partial thinking".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "text before ");
    }
}
