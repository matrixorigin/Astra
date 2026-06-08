//! File-path styling helpers — split a path into dimmed directory and
//! bright filename spans so the eye can quickly parse file references
//! in tool output, diff headers, and edited-file labels.

use ratatui::{style::Style, text::Span};

use super::theme;

/// Split `path` into directory (dim) and filename (bright) spans.
///
/// Examples:
/// ```text
/// "src/tui/tool.rs"   → [dim:"src/tui/", bright:"tool.rs"]
/// "tool.rs"           → [bright:"tool.rs"]
/// "src/tui/"          → [dim:"src/tui/"]
/// ```
///
/// Windows `\` separators are normalised to `/` for consistent styling.
pub fn style_file_path(path: &str) -> Vec<Span<'static>> {
    let theme = theme::current();
    let dim_style = theme.path_dim_style();
    let file_style = theme.path_file_style();

    // Normalise Windows separators.
    let normalised = path.replace('\\', "/");
    let path_str = normalised.as_str();

    // Find the last `/` to split directory from filename.
    if let Some(last_slash) = path_str.rfind('/') {
        let dir = &path_str[..=last_slash]; // includes trailing `/`
        let file = &path_str[last_slash + 1..];
        if file.is_empty() {
            // Path ends with `/` — treat whole thing as directory.
            vec![Span::styled(dir.to_string(), dim_style)]
        } else {
            vec![
                Span::styled(dir.to_string(), dim_style),
                Span::styled(file.to_string(), file_style),
            ]
        }
    } else {
        // No directory separator — the whole thing is a filename.
        vec![Span::styled(path_str.to_string(), file_style)]
    }
}

/// Style a single span as a file path — directories dim, filename bright.
/// Convenience wrapper that joins `style_file_path` spans into one `Line`.
pub fn style_file_path_line(path: &str) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(style_file_path(path))
}

/// Style a file path as a single `Span` with a flat style. Used when
/// the caller needs to embed the styled path inside a larger span
/// (e.g. inside a `Line` with a prefix). Prefer `style_file_path` when
/// you can use multiple spans.
pub fn style_file_path_flat(path: &str, base_style: Style) -> Vec<Span<'static>> {
    let theme = theme::current();
    let dim = base_style.fg(theme.path_dim);
    let bright = base_style.fg(theme.path_file);

    let normalised = path.replace('\\', "/");
    let path_str = normalised.as_str();

    if let Some(last_slash) = path_str.rfind('/') {
        let dir = &path_str[..=last_slash];
        let file = &path_str[last_slash + 1..];
        if file.is_empty() {
            vec![Span::styled(dir.to_string(), dim)]
        } else {
            vec![
                Span::styled(dir.to_string(), dim),
                Span::styled(file.to_string(), bright),
            ]
        }
    } else {
        vec![Span::styled(path_str.to_string(), bright)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_path_splits_directory_from_filename() {
        let spans = style_file_path("src/tui/tool.rs");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "src/tui/");
        assert_eq!(spans[1].content, "tool.rs");
    }

    #[test]
    fn filename_only_returns_single_bright_span() {
        let spans = style_file_path("Cargo.toml");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Cargo.toml");
    }

    #[test]
    fn deep_nested_path_works() {
        let spans = style_file_path("rust/crates/astra-cli/src/tui/app_event.rs");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "rust/crates/astra-cli/src/tui/");
        assert_eq!(spans[1].content, "app_event.rs");
    }

    #[test]
    fn trailing_slash_treats_whole_as_directory() {
        let spans = style_file_path("src/tui/");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "src/tui/");
    }

    #[test]
    fn windows_path_normalised() {
        let spans = style_file_path("src\\tui\\tool.rs");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "src/tui/");
        assert_eq!(spans[1].content, "tool.rs");
    }

    #[test]
    fn empty_path_handled() {
        let spans = style_file_path("");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "");
    }
}
