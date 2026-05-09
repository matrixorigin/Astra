use super::render::highlight::highlight_code_to_lines;
use super::render::line_utils::line_to_static;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

struct MarkdownStyles {
    h1: Style,
    h2: Style,
    h3: Style,
    code: Style,
    emphasis: Style,
    strong: Style,
    strikethrough: Style,
    link: Style,
    blockquote: Style,
    ordered_marker: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        Self {
            h1: Style::new().bold().underlined(),
            h2: Style::new().bold(),
            h3: Style::new().bold().italic(),
            code: Style::new().cyan(),
            emphasis: Style::new().italic(),
            strong: Style::new().bold(),
            strikethrough: Style::new().crossed_out(),
            link: Style::new().cyan().underlined(),
            blockquote: Style::new().green(),
            ordered_marker: Style::new().light_blue(),
        }
    }
}

#[allow(dead_code)]
pub(crate) fn render_markdown_text(input: &str) -> Text<'static> {
    render_markdown_text_with_width(input, None)
}

pub(crate) fn render_markdown_text_with_width(input: &str, width: Option<usize>) -> Text<'static> {
    let cwd = std::env::current_dir().ok();
    render_markdown_text_with_width_and_cwd(input, width, cwd.as_deref())
}

pub(crate) fn render_markdown_text_with_width_and_cwd(
    input: &str,
    width: Option<usize>,
    _cwd: Option<&Path>,
) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(input, options);
    let mut writer = Writer::new(width);
    writer.run(parser);
    writer.into_text()
}

struct Writer {
    styles: MarkdownStyles,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    in_code_block: Option<String>,
    code_block_content: String,
    list_stack: Vec<Option<u64>>,
    in_heading: Option<HeadingLevel>,
    in_blockquote: bool,
    link_url: Option<String>,
    /// Total render width, used for horizontal rules and wrap-aware
    /// tables. `None` means "no hint — fall back to a reasonable default".
    width: Option<usize>,
    /// Active table state. `None` outside of `Tag::Table`.
    table: Option<TableBuilder>,
}

/// Incremental collector for a GFM pipe-table. Rows are accumulated
/// as flat spans (we keep simple styling — bold/italic/code); cell
/// boundaries are tracked so that `render()` can measure widths and
/// emit a box-drawn grid on end-of-table.
struct TableBuilder {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Cells in the row currently being assembled.
    current_row: Vec<Vec<Span<'static>>>,
    /// Spans of the cell currently being filled — flushed to
    /// `current_row` at `TagEnd::TableCell`.
    current_cell: Vec<Span<'static>>,
    in_header: bool,
}

impl TableBuilder {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: Vec::new(),
            in_header: false,
        }
    }
}

impl Writer {
    fn new(width: Option<usize>) -> Self {
        Self {
            styles: MarkdownStyles::default(),
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![Style::default()],
            in_code_block: None,
            code_block_content: String::new(),
            list_stack: Vec::new(),
            in_heading: None,
            in_blockquote: false,
            link_url: None,
            width,
            table: None,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        let merged = self.current_style().patch(style);
        self.style_stack.push(merged);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        if !spans.is_empty() {
            // Blockquote lines get a `│ ` gutter in the quote colour
            // so they scan like a pull-quote rather than plain green
            // text. Body colour is already the blockquote style
            // (pushed on start_tag).
            if self.in_blockquote {
                let mut with_bar: Vec<Span<'static>> =
                    Vec::with_capacity(spans.len() + 1);
                with_bar.push(Span::styled(
                    "│ ",
                    self.styles.blockquote,
                ));
                with_bar.extend(spans);
                self.lines.push(Line::from(with_bar));
            } else {
                self.lines.push(Line::from(spans));
            }
        } else {
            self.lines.push(Line::default());
        }
    }

    fn emit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        // Inside a table cell, soft-break the text into a single span;
        // the grid renderer will wrap across lines based on column width.
        if let Some(ref mut t) = self.table {
            let flat = text.replace('\n', " ");
            t.current_cell.push(Span::styled(flat, style));
            return;
        }
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            if !part.is_empty() {
                self.current_spans
                    .push(Span::styled(part.to_string(), style));
            }
        }
    }

    /// Emit the buffered `TableBuilder` as a box-drawn table.
    ///
    /// Strategy: compute each column's natural width (longest cell),
    /// then shrink proportionally if the total exceeds the render
    /// budget so the table still fits on screen. Cells that don't fit
    /// are word-wrapped onto extra row lines.
    fn flush_table(&mut self, t: TableBuilder) {
        let ncols = t
            .header
            .len()
            .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }

        // Natural width = longest line (in display cells) from any row
        // in that column, bounded by a minimum of 3 so headers and
        // single-char cells still get a visible cell.
        let mut col_widths: Vec<usize> = vec![3; ncols];
        let measure = |spans: &[Span<'static>]| -> usize {
            spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum()
        };
        for (i, cell) in t.header.iter().enumerate() {
            if i < ncols {
                col_widths[i] = col_widths[i].max(measure(cell));
            }
        }
        for row in &t.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < ncols {
                    col_widths[i] = col_widths[i].max(measure(cell));
                }
            }
        }

        // Budget: each column has 2 padding chars + 1 vertical rule.
        // Total overhead is ncols*3 + 1 (leading border).
        let term_w = self.width.unwrap_or(80).max(20);
        let overhead = ncols * 3 + 1;
        let content_budget = term_w.saturating_sub(overhead);
        let natural_total: usize = col_widths.iter().sum();
        if natural_total > content_budget && natural_total > 0 {
            let scale = content_budget as f32 / natural_total as f32;
            for w in col_widths.iter_mut() {
                let scaled = ((*w as f32) * scale).floor() as usize;
                *w = scaled.max(3);
            }
            // One more pass to bring total down in case flooring left slack.
            let mut total: usize = col_widths.iter().sum();
            while total > content_budget {
                let idx = col_widths
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, w)| **w)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                if col_widths[idx] <= 3 {
                    break;
                }
                col_widths[idx] -= 1;
                total -= 1;
            }
        }

        self.lines.push(border_line(&col_widths, '┌', '┬', '┐'));
        if !t.header.is_empty() {
            for line in wrap_row(&t.header, &col_widths, &t.alignments, true, &self.styles) {
                self.lines.push(line);
            }
            self.lines.push(border_line(&col_widths, '├', '┼', '┤'));
        }
        for (i, row) in t.rows.iter().enumerate() {
            for line in wrap_row(row, &col_widths, &t.alignments, false, &self.styles) {
                self.lines.push(line);
            }
            if i + 1 < t.rows.len() {
                // Rule between body rows is skipped to keep the grid
                // compact; header/body boundary keeps its separator.
            }
        }
        self.lines.push(border_line(&col_widths, '└', '┴', '┘'));
        self.lines.push(Line::default());
    }

    fn flush_code_block(&mut self) {
        let lang = self.in_code_block.take().unwrap_or_default();
        let content = std::mem::take(&mut self.code_block_content);
        let content = content.trim_end_matches('\n');

        if content.is_empty() {
            return;
        }

        let highlighted = if !lang.is_empty() {
            highlight_code_to_lines(content, &lang)
        } else {
            content.lines().map(|l| Line::raw(l.to_string())).collect()
        };

        self.lines.push(Line::default());
        for line in highlighted {
            self.lines.push(line_to_static(&line));
        }
        self.lines.push(Line::default());
    }

    fn run(&mut self, parser: Parser<'_>) {
        for event in parser {
            match event {
                Event::Start(tag) => self.start_tag(tag),
                Event::End(tag) => self.end_tag(tag),
                Event::Text(text) => {
                    if self.in_code_block.is_some() {
                        self.code_block_content.push_str(&text);
                    } else {
                        self.emit_text(&text);
                    }
                }
                Event::Code(code) => {
                    // Cursor/Claude-Code convention: strip the literal
                    // backticks and rely on the cyan code style to
                    // communicate "this is code". A previous version
                    // emitted `format!("`{code}`")` which left the
                    // backticks visible in scrollback — they read as
                    // noise next to the already-coloured span.
                    let style = self.current_style().patch(self.styles.code);
                    let span = Span::styled(code.to_string(), style);
                    if let Some(ref mut t) = self.table {
                        t.current_cell.push(span);
                    } else {
                        self.current_spans.push(span);
                    }
                }
                Event::SoftBreak => {
                    self.current_spans.push(Span::raw(" "));
                }
                Event::HardBreak => {
                    self.flush_line();
                }
                Event::Rule => {
                    self.flush_line();
                    // Width defaults to a reasonable 60 when the caller
                    // doesn't know; panels that pass a real width get a
                    // rule that spans their available area.
                    let w = self.width.unwrap_or(60).max(8);
                    self.lines
                        .push(Line::styled("─".repeat(w), Style::default().dark_gray()));
                    self.flush_line();
                }
                _ => {}
            }
        }

        if !self.current_spans.is_empty() {
            self.flush_line();
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.in_heading = Some(level);
                let style = match level {
                    HeadingLevel::H1 => self.styles.h1,
                    HeadingLevel::H2 => self.styles.h2,
                    _ => self.styles.h3,
                };
                self.push_style(style);
            }
            Tag::Paragraph if !self.lines.is_empty() && self.list_stack.is_empty() => {
                if let Some(last) = self.lines.last() {
                    if !last.spans.is_empty() {
                        self.lines.push(Line::default());
                    }
                }
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.in_code_block = Some(lang);
                self.code_block_content.clear();
            }
            Tag::List(start) => {
                self.list_stack.push(start);
            }
            Tag::Item => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{indent}{}. ", n);
                        *n += 1;
                        self.current_spans
                            .push(Span::styled(marker, self.styles.ordered_marker));
                    }
                    _ => {
                        // Nested unordered lists step through ●→◦→▸→·
                        // so readers can see the depth without
                        // counting indents.
                        let glyph = match depth {
                            0 => "• ",
                            1 => "◦ ",
                            2 => "▸ ",
                            _ => "· ",
                        };
                        let marker = format!("{indent}{glyph}");
                        self.current_spans.push(Span::raw(marker));
                    }
                }
            }
            Tag::Emphasis => {
                self.push_style(self.styles.emphasis);
            }
            Tag::Strong => {
                self.push_style(self.styles.strong);
            }
            Tag::Strikethrough => {
                self.push_style(self.styles.strikethrough);
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
                self.push_style(self.styles.link);
            }
            Tag::BlockQuote(_) => {
                self.in_blockquote = true;
                self.push_style(self.styles.blockquote);
            }
            Tag::Table(alignments) => {
                // Break from any open paragraph so the table doesn't
                // glue onto preceding text.
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.table = Some(TableBuilder::new(alignments));
            }
            Tag::TableHead => {
                if let Some(ref mut t) = self.table {
                    t.in_header = true;
                    t.current_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(ref mut t) = self.table {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(ref mut t) = self.table {
                    t.current_cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.in_heading = None;
                self.pop_style();
                self.flush_line();
            }
            TagEnd::Paragraph => {
                self.flush_line();
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.lines.push(Line::default());
                }
            }
            TagEnd::Item if !self.current_spans.is_empty() => {
                self.flush_line();
            }
            TagEnd::Emphasis => {
                self.pop_style();
            }
            TagEnd::Strong => {
                self.pop_style();
            }
            TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
                if let Some(url) = self.link_url.take() {
                    if !url.is_empty() {
                        self.current_spans.push(Span::styled(
                            format!(" ({url})"),
                            Style::default().dark_gray(),
                        ));
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                self.in_blockquote = false;
                self.pop_style();
            }
            TagEnd::TableCell => {
                if let Some(ref mut t) = self.table {
                    let cell = std::mem::take(&mut t.current_cell);
                    t.current_row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(ref mut t) = self.table {
                    let row = std::mem::take(&mut t.current_row);
                    t.header = row;
                    t.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(ref mut t) = self.table {
                    let row = std::mem::take(&mut t.current_row);
                    if !row.is_empty() {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.flush_table(t);
                }
            }
            _ => {}
        }
    }

    fn into_text(mut self) -> Text<'static> {
        while self.lines.last().is_some_and(|l| l.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

fn border_line(col_widths: &[usize], left: char, mid: char, right: char) -> Line<'static> {
    let mut s = String::new();
    s.push(left);
    for (i, w) in col_widths.iter().enumerate() {
        for _ in 0..(w + 2) {
            s.push('─');
        }
        if i + 1 < col_widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    Line::styled(s, Style::default().dark_gray())
}

/// Wrap a single logical row into one or more visual lines inside the
/// grid. Cells are clipped to their column width; longer cells spill
/// into additional wrap lines (word-aware, whitespace split).
fn wrap_row(
    cells: &[Vec<Span<'static>>],
    col_widths: &[usize],
    alignments: &[Alignment],
    is_header: bool,
    styles: &MarkdownStyles,
) -> Vec<Line<'static>> {
    // Concatenate each cell's spans into one string for wrap math, but
    // keep the original span list to preserve styling. Wrapping by
    // characters is the simplest correct thing for a first pass and
    // keeps alignment math predictable — per-span colour fidelity can
    // come later.
    let dim = Style::default().dark_gray();
    let cell_texts: Vec<String> = cells
        .iter()
        .map(|c| c.iter().map(|s| s.content.to_string()).collect::<String>())
        .collect();

    let wrapped: Vec<Vec<String>> = cell_texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let w = col_widths.get(i).copied().unwrap_or(10);
            wrap_cell(text, w)
        })
        .collect();

    let line_count = wrapped.iter().map(|v| v.len()).max().unwrap_or(1).max(1);

    let mut out: Vec<Line<'static>> = Vec::with_capacity(line_count);
    for row in 0..line_count {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("│ ", dim));
        for (i, w) in col_widths.iter().enumerate() {
            let piece = wrapped
                .get(i)
                .and_then(|rows| rows.get(row))
                .map(|s| s.as_str())
                .unwrap_or("");
            let align = alignments.get(i).copied().unwrap_or(Alignment::Left);
            let padded = pad_cell(piece, *w, align);
            let mut cell_style = Style::default();
            if is_header {
                cell_style = cell_style.patch(styles.strong);
            }
            spans.push(Span::styled(padded, cell_style));
            if i + 1 < col_widths.len() {
                spans.push(Span::styled(" │ ", dim));
            } else {
                spans.push(Span::styled(" │", dim));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    if UnicodeWidthStr::width(text) <= width {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let ww = UnicodeWidthStr::width(word);
        if ww >= width {
            // Hard-break a giant word.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_w = 0;
            }
            let mut tmp = String::new();
            let mut tw = 0;
            for ch in word.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if tw + cw > width && !tmp.is_empty() {
                    out.push(std::mem::take(&mut tmp));
                    tw = 0;
                }
                tmp.push(ch);
                tw += cw;
            }
            if !tmp.is_empty() {
                current = tmp;
                current_w = tw;
            }
            continue;
        }
        let sep = if current.is_empty() { 0 } else { 1 };
        if current_w + sep + ww > width {
            out.push(std::mem::take(&mut current));
            current_w = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_w += 1;
        }
        current.push_str(word);
        current_w += ww;
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn pad_cell(text: &str, width: usize, align: Alignment) -> String {
    let tw = UnicodeWidthStr::width(text);
    if tw >= width {
        return text.to_string();
    }
    let pad = width - tw;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), text),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        _ => format!("{}{}", text, " ".repeat(pad)),
    }
}

#[cfg(test)]
mod polish_tests {
    use super::*;

    fn lines(md: &str) -> Vec<String> {
        render_markdown_text(md)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    fn lines_at(md: &str, width: usize) -> Vec<String> {
        render_markdown_text_with_width(md, Some(width))
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn blockquote_has_vertical_bar_prefix() {
        let out = lines("> quoted text\n> second line");
        assert!(
            out.iter().any(|l| l.starts_with("│ ")),
            "blockquote should start with a vertical bar; got {out:?}"
        );
    }

    #[test]
    fn nested_unordered_lists_use_tiered_glyphs() {
        let md = "\
- outer
  - middle
    - inner
";
        let out = lines(md);
        assert!(out.iter().any(|l| l.trim_start().starts_with("• outer")));
        assert!(out.iter().any(|l| l.trim_start().starts_with("◦ middle")));
        assert!(out.iter().any(|l| l.trim_start().starts_with("▸ inner")));
    }

    #[test]
    fn nested_list_depth_is_indented() {
        let md = "\
- outer
  - middle
";
        let out = lines(md);
        let middle = out
            .iter()
            .find(|l| l.contains("middle"))
            .expect("middle line");
        // "  " per depth level, depth=1 → 2 spaces before the glyph.
        assert!(middle.starts_with("  ◦ "), "got: {middle:?}");
    }

    #[test]
    fn horizontal_rule_matches_supplied_width() {
        let out = lines_at("before\n\n---\n\nafter", 30);
        let rule_line = out
            .iter()
            .find(|l| l.chars().all(|c| c == '─') && !l.is_empty())
            .expect("rule line");
        assert_eq!(rule_line.chars().count(), 30);
    }

    #[test]
    fn table_renders_with_box_borders() {
        let md = "\
| col a | col b |
|-------|-------|
| one   | two   |
| three | four  |
";
        let out = lines_at(md, 40);
        let joined = out.join("\n");
        assert!(joined.contains("┌"), "missing top-left corner: {joined}");
        assert!(joined.contains("├"), "missing header separator: {joined}");
        assert!(joined.contains("└"), "missing bottom-left corner: {joined}");
        assert!(joined.contains("col a"), "header missing: {joined}");
        assert!(joined.contains("three"), "body row missing: {joined}");
    }

    #[test]
    fn table_columns_shrink_to_fit_terminal_width() {
        let md = "\
| a long header | another long one |
|---------------|------------------|
| cell content  | more content     |
";
        // Tight width — columns must shrink and body must wrap.
        let out = lines_at(md, 28);
        let grid_lines: Vec<&String> =
            out.iter().filter(|l| l.starts_with('│') || l.starts_with('┌') || l.starts_with('├') || l.starts_with('└')).collect();
        assert!(!grid_lines.is_empty(), "no grid lines produced: {out:?}");
        for l in &grid_lines {
            assert!(
                l.chars().count() <= 30,
                "table line exceeded terminal width: {:?}",
                l
            );
        }
    }

    #[test]
    fn horizontal_rule_fallbacks_to_default_when_no_width() {
        let out = lines("a\n\n---\n\nb");
        let rule_line = out
            .iter()
            .find(|l| l.chars().all(|c| c == '─') && !l.is_empty())
            .expect("rule line");
        // Previous implementation was hardcoded to 40 — now it's 60.
        assert_eq!(rule_line.chars().count(), 60);
    }
}
