use std::borrow::Cow;

use ratatui::text::{Line, Span};

fn is_unsafe_terminal_control(ch: char) -> bool {
    ch.is_control() && ch != '\n' && ch != '\t'
}

pub(crate) fn sanitize_terminal_text(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_unsafe_terminal_control) {
        return Cow::Borrowed(text);
    }

    let mut sanitized = String::with_capacity(text.len());
    for ch in text.chars() {
        if !is_unsafe_terminal_control(ch) {
            sanitized.push(ch);
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

#[allow(dead_code)]
pub(crate) fn prefix_lines<'a>(
    lines: impl IntoIterator<Item = Line<'a>>,
    initial: Line<'a>,
    subsequent: Line<'a>,
) -> Vec<Line<'a>> {
    let mut result = Vec::new();
    for (i, mut line) in lines.into_iter().enumerate() {
        let prefix = if i == 0 {
            initial.clone()
        } else {
            subsequent.clone()
        };
        let mut spans = prefix.spans;
        spans.append(&mut line.spans);
        result.push(Line::from(spans));
    }
    result
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::*;

    #[test]
    fn sanitize_terminal_text_strips_control_sequences_but_keeps_newlines_and_tabs() {
        let text = "ok\x1b[31m\tstill\nfine\r\u{009b}1m";
        assert_eq!(sanitize_terminal_text(text), "ok[31m\tstill\nfine1m");
    }

    #[test]
    fn sanitize_line_for_terminal_preserves_style_and_alignment() {
        let mut line = Line::from(vec![
            Span::styled("safe\x1b[31m", Style::default().fg(Color::Green)),
            Span::raw("\tb\t"),
        ]);
        line.style = Style::default().bg(Color::Black);
        line.alignment = Some(ratatui::layout::Alignment::Center);

        let sanitized = sanitize_line_for_terminal(&line);
        assert_eq!(sanitized.spans.len(), 2);
        assert_eq!(sanitized.spans[0].content.as_ref(), "safe[31m");
        assert_eq!(sanitized.spans[1].content.as_ref(), "\tb\t");
        assert_eq!(sanitized.spans[0].style, Style::default().fg(Color::Green));
        assert_eq!(sanitized.style, Style::default().bg(Color::Black));
        assert_eq!(
            sanitized.alignment,
            Some(ratatui::layout::Alignment::Center)
        );
    }
}
