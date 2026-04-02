//! Incremental markdown streaming renderer.
//!
//! Inspired by claudecode's `StreamingMarkdown` component: split accumulated
//! text at the last complete top-level block boundary.  Everything before the
//! boundary is "stable" — rendered once and never touched again.  Only the
//! trailing "unstable" block is cleared and re-rendered on each delta.
//!
//! Uses [`TerminalRegion`] for flicker-free diff-based updates.

use termimad::crossterm::style::Color;
use termimad::{FmtText, MadSkin};

use super::terminal_region::TerminalRegion;

/// Incremental markdown renderer that streams formatted output.
pub(super) struct StreamingMarkdown {
    /// Full accumulated text so far.
    full_text: String,
    /// Byte offset into `full_text` up to which we have already printed
    /// stable (finalized) markdown blocks.
    stable_end: usize,
    /// Terminal width for rendering.
    term_width: usize,
    /// Stable region — already finalized, only appended to.
    stable_region: TerminalRegion,
    /// Unstable region — cleared and re-rendered on each delta.
    unstable_region: TerminalRegion,
}

impl StreamingMarkdown {
    pub(super) fn new(term_width: usize) -> Self {
        Self {
            full_text: String::new(),
            stable_end: 0,
            term_width: term_width.max(20),
            stable_region: TerminalRegion::new(),
            unstable_region: TerminalRegion::new(),
        }
    }

    /// Total lines currently on screen (stable + unstable).
    #[allow(dead_code)]
    pub(super) fn height(&self) -> usize {
        self.stable_region.height() + self.unstable_region.height()
    }

    /// Append a text delta and incrementally render.
    pub(super) fn push(&mut self, delta: &str) {
        self.full_text.push_str(delta);

        // Strip XML-style thinking/reflect tags that leaked into text output.
        let old_len = self.full_text.len();
        strip_xml_tags_inplace(&mut self.full_text);
        if self.full_text.len() < old_len {
            self.stable_end = self.stable_end.min(self.full_text.len());
        }

        // Throttle: only re-render on newlines or large deltas.
        if !delta.contains('\n') && delta.len() < 20 {
            return;
        }

        self.render_incremental();
    }

    fn render_incremental(&mut self) {
        let new_stable_end = find_last_block_boundary(&self.full_text);

        // If stable region grew, commit newly-stable lines.
        if new_stable_end > self.stable_end {
            // Clear unstable region first.
            self.unstable_region.clear();

            // Render newly-stable markdown and append to stable region.
            let new_stable = &self.full_text[self.stable_end..new_stable_end];
            let rendered = render_md(new_stable, self.term_width);
            let lines = rendered_to_lines(&rendered);
            self.stable_region.append(&lines);
            self.stable_end = new_stable_end;
        }

        // Render the unstable suffix via diff-update (no flicker).
        let unstable = &self.full_text[self.stable_end..];
        if !unstable.is_empty() {
            let rendered = render_md(unstable, self.term_width);
            let lines = rendered_to_lines(&rendered);
            self.unstable_region.update(lines);
        } else {
            self.unstable_region.update(Vec::new());
        }
    }

    /// Finalize: render any buffered content.
    pub(super) fn finish(&mut self) {
        self.render_incremental();
    }

    /// Drop any intermediate draft before the next tool round.
    /// For multi-turn tool workflows we only want the final answer to remain
    /// visible; preserving draft prose makes reviews appear duplicated.
    pub(super) fn discard_and_reset(&mut self) {
        self.unstable_region.clear();
        self.stable_region.clear();
        self.stable_end = 0;
        self.full_text.clear();
    }

    /// Clear ALL rendered output (stable + unstable).
    /// Only used when output must be fully removed.
    #[allow(dead_code)]
    pub(super) fn clear_all(&mut self) {
        self.unstable_region.clear();
        self.stable_region.clear();
        self.stable_end = 0;
        self.full_text.clear();
    }
}

/// Split rendered markdown into terminal lines (stripping trailing newline).
fn rendered_to_lines(rendered: &str) -> Vec<String> {
    if rendered.is_empty() {
        return Vec::new();
    }
    // FmtText ends each line with \n. Split and drop the trailing empty.
    let mut lines: Vec<String> = rendered.split('\n').map(String::from).collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Strip LLM thinking/reflect XML tags from text in-place.
///
/// Handles:
/// - Matched pairs: `<think>content</think>` → removed entirely
/// - Unclosed opening tags: `<think>trailing` → truncated at tag start
/// - Lone closing tags: `</think>` without matching open → removed
pub(super) fn strip_xml_tags_inplace(text: &mut String) {
    const TAGS: &[&str] = &["reflect", "thinking", "think", "inner_monologue"];
    let mut changed = false;
    for tag in TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        // Strip matched pairs first.
        while let Some(start) = text.find(&open) {
            if let Some(end) = text[start..].find(&close) {
                let remove_end = start + end + close.len();
                let remove_end = if text.as_bytes().get(remove_end) == Some(&b'\n') {
                    remove_end + 1
                } else {
                    remove_end
                };
                text.drain(start..remove_end);
                changed = true;
            } else {
                text.truncate(start);
                changed = true;
                break;
            }
        }
        // Strip any remaining lone closing tags (model may leak `</think>`
        // without a matching opening tag in the same text block).
        while let Some(pos) = text.find(&close) {
            let remove_end = pos + close.len();
            let remove_end = if text.as_bytes().get(remove_end) == Some(&b'\n') {
                remove_end + 1
            } else {
                remove_end
            };
            text.drain(pos..remove_end);
            changed = true;
        }
    }
    if changed {
        while text.contains("\n\n\n") {
            *text = text.replace("\n\n\n", "\n\n");
        }
    }
}

/// Strip leading narration from text in-place.
///
/// LLMs sometimes prepend phrases like "Now I have enough context..." before
/// the actual answer.  This function removes such preambles by detecting:
/// - Lines starting with common narration patterns
/// - Keeps content starting from markdown structure (headers, bold, lists)
pub(super) fn strip_leading_narration(text: &mut String) {
    // Patterns that indicate narration (case-insensitive matching)
    const NARRATION_STARTS: &[&str] = &[
        "now i have",
        "now let me",
        "let me ",
        "i'll ",
        "i will ",
        "i need to",
        "i can see",
        "i can now",
        "based on ",
        "looking at ",
    ];

    // Patterns that indicate actual content (should NOT be stripped)
    const CONTENT_MARKERS: &[&str] = &[
        "**",   // Bold (common for headers like **Summary**)
        "# ",   // Markdown header
        "## ",  // Markdown header
        "### ", // Markdown header
        "- ",   // List item
        "* ",   // List item
        "1. ",  // Numbered list
        "```",  // Code block
        "| ",   // Table
        "---",  // Horizontal rule
        "___",  // Horizontal rule
    ];

    // If text starts with content marker, don't touch it
    let trimmed = text.trim_start();
    for marker in CONTENT_MARKERS {
        if trimmed.starts_with(marker) {
            return;
        }
    }

    // Check if we start with narration
    let lower = trimmed.to_lowercase();
    let mut is_narration = false;
    for pattern in NARRATION_STARTS {
        if lower.starts_with(pattern) {
            is_narration = true;
            break;
        }
    }

    if !is_narration {
        return;
    }

    // Find the first content marker and strip everything before it
    let mut earliest_content = None;
    for marker in CONTENT_MARKERS {
        if let Some(pos) = text.find(marker) {
            match earliest_content {
                None => earliest_content = Some(pos),
                Some(prev) if pos < prev => earliest_content = Some(pos),
                _ => {}
            }
        }
    }

    if let Some(pos) = earliest_content {
        // Find the start of the line containing the content marker
        let line_start = text[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0);
        if line_start > 0 {
            text.drain(..line_start);
        }
    }
}

/// Returns `true` when `text` contains an opened but not-yet-closed XML tag
/// from the known set of LLM thinking tags.  Used to suppress premature
/// rendering of text that will be stripped once the closing tag arrives.
pub(super) fn has_open_xml_tag(text: &str) -> bool {
    const TAGS: &[&str] = &["reflect", "thinking", "think", "inner_monologue"];
    for tag in TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        // Walk all opens; if any isn't matched by a close, we have an open tag.
        let mut search_from = 0;
        loop {
            let Some(start) = text[search_from..].find(&open) else {
                break;
            };
            let abs = search_from + start;
            if text[abs..].find(&close).is_none() {
                return true;
            }
            search_from = abs + open.len();
        }
    }
    false
}

fn make_skin() -> MadSkin {
    let mut skin = MadSkin::default();
    skin.headers[0].set_fg(Color::Cyan);
    skin.headers[1].set_fg(Color::Cyan);
    skin.bold.set_fg(Color::White);
    skin.italic.set_fg(Color::Magenta);
    skin
}

fn render_md(text: &str, width: usize) -> String {
    let skin = make_skin();
    let fmt = FmtText::from(&skin, text, Some(width));
    format!("{fmt}")
}

/// Find the byte offset of the last "stable" block boundary.
fn find_last_block_boundary(text: &str) -> usize {
    let mut last = 0;
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find("\n\n") {
        let abs = search_from + pos + 2; // byte after the double-newline
        if abs < text.len() && is_block_start(&text[abs..]) {
            last = abs;
        }
        search_from = abs;
    }
    last
}

fn is_block_start(text: &str) -> bool {
    let first = text.chars().next().unwrap_or(' ');
    match first {
        '#' => true,
        '-' | '*' | '+' => text.len() > 1 && text.as_bytes()[1] == b' ',
        '>' => true,
        '`' => text.starts_with("```"),
        '|' => true,
        '0'..='9' => text.len() > 1 && (text.as_bytes()[1] == b'.' || text.as_bytes()[1] == b')'),
        _ => true,
    }
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
        assert!(b > 0);
        assert_eq!(&text[b..], "second paragraph");
    }

    #[test]
    fn boundary_at_heading() {
        let text = "some text\n\n## Heading\nmore";
        let b = find_last_block_boundary(text);
        assert!(b > 0);
        assert!(&text[b..].starts_with("## Heading"));
    }

    #[test]
    fn boundary_at_code_fence() {
        let text = "text\n\n```rust\nfn main() {}\n```";
        let b = find_last_block_boundary(text);
        assert!(b > 0);
        assert!(&text[b..].starts_with("```rust"));
    }

    #[test]
    fn count_rendered_lines() {
        let lines = rendered_to_lines("hello\nworld\n");
        assert_eq!(lines, vec!["hello", "world"]);
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
    fn push_renders_unstable_text_before_first_block_boundary() {
        let mut sm = StreamingMarkdown::new(80);
        sm.push("this is a long opening line");
        assert_eq!(sm.stable_end, 0);
        assert!(sm.unstable_region.height() > 0);
    }

    #[test]
    fn push_renders_on_newline_without_paragraph_break() {
        let mut sm = StreamingMarkdown::new(80);
        sm.push("Summary:\n");
        assert_eq!(sm.stable_end, 0);
        assert!(sm.unstable_region.height() > 0);
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

    #[test]
    fn strip_think_tags() {
        let mut s = "before\n<think>\nlong thinking block\n</think>\nafter".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "before\nafter");
    }

    #[test]
    fn strip_partial_think_tag() {
        let mut s = "text <think>still thinking...".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "text ");
    }

    #[test]
    fn has_open_xml_tag_detects_think() {
        assert!(has_open_xml_tag("<think>some content"));
        assert!(!has_open_xml_tag("<think>some content</think>"));
        assert!(!has_open_xml_tag("no tags here"));
    }

    #[test]
    fn has_open_xml_tag_detects_reflect() {
        assert!(has_open_xml_tag("text <reflect>partial"));
        assert!(!has_open_xml_tag("text <reflect>done</reflect>"));
    }

    #[test]
    fn has_open_xml_tag_handles_multiple_tags() {
        assert!(!has_open_xml_tag("<think>a</think><think>b</think>"));
        assert!(has_open_xml_tag("<think>a</think><think>still open"));
    }

    #[test]
    fn strip_lone_closing_think_tag() {
        let mut s = "some reasoning content\n</think>\nactual response".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "some reasoning content\nactual response");
    }

    #[test]
    fn strip_lone_closing_reflect_tag() {
        let mut s = "draft output</reflect>final".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "draft outputfinal");
    }

    #[test]
    fn strip_lone_closing_tag_with_matched_pair() {
        // Matched pair stripped first, then lone closing tag stripped.
        let mut s = "<think>hidden</think>visible</think>more".to_string();
        strip_xml_tags_inplace(&mut s);
        assert_eq!(s, "visiblemore");
    }

    #[test]
    fn strip_leading_narration_removes_preamble() {
        let mut s = "Now I have enough context.\n\n**Summary**: The change is good.".to_string();
        strip_leading_narration(&mut s);
        assert_eq!(s, "**Summary**: The change is good.");
    }

    #[test]
    fn strip_leading_narration_preserves_content_start() {
        let mut s = "**Summary**: The change is good.".to_string();
        strip_leading_narration(&mut s);
        assert_eq!(s, "**Summary**: The change is good.");
    }

    #[test]
    fn strip_leading_narration_removes_let_me() {
        let mut s = "Let me analyze this.\n\n# Review\n\nLooks good.".to_string();
        strip_leading_narration(&mut s);
        assert_eq!(s, "# Review\n\nLooks good.");
    }

    #[test]
    fn strip_leading_narration_keeps_non_narration() {
        let mut s = "This PR adds a new feature.\n\n**Details**: ...".to_string();
        strip_leading_narration(&mut s);
        assert_eq!(s, "This PR adds a new feature.\n\n**Details**: ...");
    }

    #[test]
    fn strip_leading_narration_handles_list_content() {
        let mut s = "Based on my analysis:\n\n- Item 1\n- Item 2".to_string();
        strip_leading_narration(&mut s);
        assert_eq!(s, "- Item 1\n- Item 2");
    }
}
