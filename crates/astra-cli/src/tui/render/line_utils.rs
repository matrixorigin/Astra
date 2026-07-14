use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};

const MAX_STRING_ESCAPE_SCAN_CHARS: usize = 256;

fn is_unsafe_terminal_control(ch: char) -> bool {
    ch.is_control() && ch != '\n' && ch != '\t'
}

fn skip_csi(chars: &mut Peekable<Chars<'_>>) {
    for ch in chars.by_ref() {
        if ('@'..='~').contains(&ch) {
            break;
        }
    }
}

fn skip_string_escape(chars: &mut Peekable<Chars<'_>>, allow_bel: bool) -> Option<char> {
    let mut scanned = 0usize;
    while let Some(ch) = chars.next() {
        scanned += 1;
        if allow_bel && ch == '\x07' {
            return None;
        }
        if ch == '\u{009c}' {
            return None;
        }
        if ch == '\x1b' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            return None;
        }
        if ch == '\n' {
            return Some('\n');
        }
        if scanned >= MAX_STRING_ESCAPE_SCAN_CHARS {
            return (!is_unsafe_terminal_control(ch)).then_some(ch);
        }
    }
    None
}

pub(crate) fn sanitize_terminal_text(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|ch| {
        is_unsafe_terminal_control(ch)
            || matches!(
                ch,
                '\x1b'
                    | '\u{0090}'
                    | '\u{0098}'
                    | '\u{009b}'
                    | '\u{009c}'
                    | '\u{009d}'
                    | '\u{009e}'
                    | '\u{009f}'
            )
    }) {
        return Cow::Borrowed(text);
    }

    let mut sanitized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    skip_csi(&mut chars);
                }
                Some(']') => {
                    chars.next();
                    if let Some(ch) = skip_string_escape(&mut chars, true) {
                        sanitized.push(ch);
                    }
                }
                Some('P' | 'X' | '^' | '_') => {
                    chars.next();
                    if let Some(ch) = skip_string_escape(&mut chars, false) {
                        sanitized.push(ch);
                    }
                }
                Some(next) if ('@'..='_').contains(next) => {
                    chars.next();
                }
                _ => {}
            },
            '\u{009b}' => skip_csi(&mut chars),
            '\u{009d}' => {
                if let Some(ch) = skip_string_escape(&mut chars, true) {
                    sanitized.push(ch);
                }
            }
            '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                if let Some(ch) = skip_string_escape(&mut chars, false) {
                    sanitized.push(ch);
                }
            }
            '\u{009c}' => {}
            _ if !is_unsafe_terminal_control(ch) => sanitized.push(ch),
            _ => {}
        }
    }
    Cow::Owned(sanitized)
}

pub(crate) fn sanitize_line_for_terminal(line: &Line<'_>) -> Line<'static> {
    let mut out = Line::from(
        line.spans
            .iter()
            .map(|span| {
                Span::styled(
                    sanitize_terminal_text(&span.content).into_owned(),
                    span.style,
                )
            })
            .collect::<Vec<_>>(),
    );
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

pub(crate) fn sanitize_lines_for_terminal(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| sanitize_line_for_terminal(&line))
        .collect()
}

pub(crate) fn line_to_static(line: &Line<'_>) -> Line<'static> {
    sanitize_line_for_terminal(line)
}

pub(crate) fn push_owned_lines(src: &[Line<'_>], out: &mut Vec<Line<'static>>) {
    for line in src {
        out.push(line_to_static(line));
    }
}

/// A [`Paragraph`] that treats `Line.style.bg` as semantic ownership of the
/// complete physical row.
///
/// Ratatui applies a line style only to cells occupied by text. That is the
/// right default for ordinary prose, but not for user-message and diff
/// surfaces: their background denotes the whole row, including the terminal
/// tail after the final glyph. Painting the buffer first preserves that
/// contract without padding content to the viewport width, which would arm a
/// real terminal's auto-wrap state at the right edge.
pub(crate) struct FullRowParagraph<'a> {
    lines: Vec<Line<'a>>,
    wrap: Option<Wrap>,
}

impl<'a> FullRowParagraph<'a> {
    pub(crate) fn new(lines: Vec<Line<'a>>) -> Self {
        Self { lines, wrap: None }
    }

    pub(crate) fn wrap(mut self, wrap: Wrap) -> Self {
        self.wrap = Some(wrap);
        self
    }
}

impl Widget for FullRowParagraph<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let mut y = area.y;
        for line in &self.lines {
            if y >= area.bottom() {
                break;
            }
            let physical_rows = self.wrap.map_or(1, |wrap| {
                Paragraph::new(line.clone())
                    .wrap(wrap)
                    .line_count(area.width)
                    .max(1) as u16
            });
            if let Some(background) = line.style.bg {
                let row_area = Rect::new(
                    area.x,
                    y,
                    area.width,
                    physical_rows.min(area.bottom().saturating_sub(y)),
                );
                buf.set_style(row_area, Style::default().bg(background));
            }
            y = y.saturating_add(physical_rows);
        }

        let mut paragraph = Paragraph::new(self.lines);
        if let Some(wrap) = self.wrap {
            paragraph = paragraph.wrap(wrap);
        }
        paragraph.render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::{FullRowParagraph, sanitize_line_for_terminal, sanitize_terminal_text};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Alignment;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Widget, Wrap};

    #[test]
    fn sanitize_terminal_text_strips_control_sequences_but_keeps_newlines_and_tabs() {
        let text = "ok\x1b[31m\tstill\nfine\r\u{009b}1m\x1b]0;title\x07";
        assert_eq!(sanitize_terminal_text(text), "ok\tstill\nfine");
    }

    #[test]
    fn sanitize_line_for_terminal_preserves_style_and_alignment() {
        let mut line = Line::from(vec![
            Span::styled("safe\x1b[31m", Style::default().fg(Color::Green)),
            Span::raw("\tb\t"),
        ]);
        line.style = Style::default().bg(Color::Black);
        line.alignment = Some(Alignment::Center);

        let sanitized = sanitize_line_for_terminal(&line);
        assert_eq!(sanitized.spans.len(), 2);
        assert_eq!(sanitized.spans[0].content.as_ref(), "safe");
        assert_eq!(sanitized.spans[1].content.as_ref(), "\tb\t");
        assert_eq!(sanitized.spans[0].style, Style::default().fg(Color::Green));
        assert_eq!(sanitized.style, Style::default().bg(Color::Black));
        assert_eq!(sanitized.alignment, Some(Alignment::Center));
    }

    #[test]
    fn sanitize_terminal_text_limits_unterminated_string_escape_damage() {
        let text = format!("prefix\x1b]0;{}tail", "x".repeat(300));
        let sanitized = sanitize_terminal_text(&text);
        assert!(sanitized.starts_with("prefix"));
        assert!(sanitized.ends_with("tail"));
    }

    #[test]
    fn sanitize_terminal_text_preserves_newline_after_unterminated_string_escape() {
        let text = "prefix\x1b]0;title\nvisible";
        assert_eq!(sanitize_terminal_text(text), "prefix\nvisible");
    }

    #[test]
    fn full_row_paragraph_paints_wrapped_rows_without_leaking_into_the_next_line() {
        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(area);
        let lines = vec![
            Line::from("123456789").style(Style::default().bg(Color::Red)),
            Line::from("next").style(Style::default().bg(Color::Blue)),
            Line::from("plain"),
        ];

        FullRowParagraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);

        assert!((0..8).all(|x| buffer[(x, 0)].bg == Color::Red));
        assert!((0..8).all(|x| buffer[(x, 1)].bg == Color::Red));
        assert!((0..8).all(|x| buffer[(x, 2)].bg == Color::Blue));
        assert!((0..8).all(|x| buffer[(x, 3)].bg == Color::Reset));
    }
}
