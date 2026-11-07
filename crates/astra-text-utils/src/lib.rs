//! Text processing utilities extracted from the runtime crate.
//!
//! Provides tokenization and lexical semantic deduplication without runtime
//! infrastructure dependencies.

pub mod credential_redaction;
pub mod line_range;
pub mod semantic_dedup;
pub mod str_preview;
pub mod text_tokenize;
pub mod tool_name;
pub mod url_component;
pub mod xml_escape;

/// Find two occurrences, including those that overlap at a UTF-8 boundary.
/// An empty needle returns `None`; callers must validate empty anchors first.
pub fn first_two_match_positions(text: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let first = text.find(needle)?;
    let next = text.ceil_char_boundary(first + 1);
    let second = next + text[next..].find(needle)?;
    Some((first, second))
}
