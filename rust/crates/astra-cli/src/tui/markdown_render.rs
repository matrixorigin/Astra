use super::render::highlight::highlight_code_to_lines;
use super::render::line_utils::line_to_static;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use std::path::Path;

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
    _width: Option<usize>,
    _cwd: Option<&Path>,
) -> Text<'static> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(input, options);
    let mut writer = Writer::new();
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
}

impl Writer {
    fn new() -> Self {
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
            self.lines.push(Line::from(spans));
        } else {
            self.lines.push(Line::default());
        }
    }

    fn emit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            if !part.is_empty() {
                self.current_spans.push(Span::styled(part.to_string(), style));
            }
        }
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
                    let style = self.current_style().patch(self.styles.code);
                    self.current_spans
                        .push(Span::styled(format!("`{code}`"), style));
                }
                Event::SoftBreak => {
                    self.current_spans.push(Span::raw(" "));
                }
                Event::HardBreak => {
                    self.flush_line();
                }
                Event::Rule => {
                    self.flush_line();
                    self.lines.push(Line::styled(
                        "─".repeat(40),
                        Style::default().dark_gray(),
                    ));
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
            Tag::Paragraph => {
                if !self.lines.is_empty() && self.list_stack.is_empty() {
                    if let Some(last) = self.lines.last() {
                        if !last.spans.is_empty() {
                            self.lines.push(Line::default());
                        }
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
                        let marker = format!("{indent}- ");
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
            TagEnd::Item => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
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
