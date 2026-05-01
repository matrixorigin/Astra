use ratatui::text::{Line, Span};

pub(crate) fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.to_string(), s.style))
            .collect::<Vec<_>>(),
    )
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
        spans.extend(line.spans.drain(..));
        result.push(Line::from(spans));
    }
    result
}
