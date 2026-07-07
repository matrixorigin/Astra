use super::markdown_render::render_markdown_text_with_width_and_cwd;
use super::render::line_utils::push_owned_lines;
use ratatui::text::Line;
use std::path::Path;

pub(crate) fn append_markdown(
    markdown_source: &str,
    width: Option<usize>,
    cwd: Option<&Path>,
    lines: &mut Vec<Line<'static>>,
) {
    let text = render_markdown_text_with_width_and_cwd(markdown_source, width, cwd);
    push_owned_lines(&text.lines, lines);
}
