//! Incremental Markdown line/container prefixes used by the DSML lexer.
//! Cursor checkpoints retain completed containers. Only bounded marker/padding
//! lookahead can be revisited; long indentation is published once it is literal.

#[derive(Clone, Debug)]
pub(super) enum Container {
    Quote,
    List(usize),
}

pub(super) struct Prefix {
    pub end: usize,
    pub indented: bool,
    pub containers: Vec<Container>,
    pub continuation_end: usize,
    pub left_context: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Position {
    byte: usize,
    column: usize,
    carry: usize,
}

impl Position {
    fn indent(&mut self, bytes: &[u8], limit: usize) -> usize {
        let mut width = std::mem::take(&mut self.carry);
        while width < limit {
            let step = match bytes.get(self.byte) {
                Some(b' ') => 1,
                Some(b'\t') => 4 - self.column % 4,
                _ => break,
            };
            self.byte += 1;
            self.column += step;
            width += step;
        }
        width
    }

    fn marker(&mut self, width: usize) {
        self.byte += width;
        self.column += width;
    }

    // A tab after `>` supplies one padding column; any remaining columns are
    // indentation within the quote, not a fresh four-column tab expansion.
    fn padding_column(&mut self, bytes: &[u8]) {
        match bytes.get(self.byte) {
            Some(b' ') => self.marker(1),
            Some(b'\t') => {
                let width = 4 - self.column % 4;
                self.byte += 1;
                self.column += width;
                self.carry = width - 1;
            }
            _ => {}
        }
    }

    fn quote(&mut self, bytes: &[u8], finish: bool) -> Option<bool> {
        if bytes.get(self.byte) != Some(&b'>') {
            return Some(false);
        }
        if self.byte + 1 == bytes.len() && !finish {
            return None;
        }
        self.marker(1);
        self.padding_column(bytes);
        Some(true)
    }
}

#[derive(Debug, Default)]
pub(super) struct Cursor {
    at: Position,
    containers: Vec<Container>,
    next: usize,
    inherited_done: bool,
    continuation_end: usize,
    left_context: bool,
}

impl Cursor {
    fn prefix(&self, at: Position, indented: bool, containers: &[Container]) -> Prefix {
        Prefix {
            end: at.byte,
            indented,
            containers: containers.to_vec(),
            continuation_end: self.continuation_end,
            left_context: self.left_context,
        }
    }

    pub(super) fn normal(
        &mut self,
        text: &str,
        finish: bool,
        inherited: &[Container],
    ) -> Option<Prefix> {
        if !self.inherited_done {
            match self.continue_containers(text, finish, inherited)? {
                Context::Blank(mut prefix) => {
                    prefix.continuation_end = prefix.end;
                    return Some(prefix);
                }
                Context::Left => {
                    self.left_context = true;
                }
                Context::Matched => {}
            }
            self.inherited_done = true;
            self.continuation_end = self.at.byte;
        }
        let bytes = text.as_bytes();
        loop {
            let mut at = self.at;
            let indent = at.indent(bytes, 4);
            // Four columns already establish literal indentation. Do not wait
            // for the end of an arbitrarily long whitespace-only stream.
            if indent >= 4 {
                return Some(self.prefix(at, true, &self.containers));
            }
            if at.byte == bytes.len() && !finish {
                return None;
            }
            if at.quote(bytes, finish)? {
                self.containers.push(Container::Quote);
                self.at = at;
                continue;
            }
            let marker = match bytes.get(at.byte) {
                Some(b'-' | b'+' | b'*') => Some(1),
                Some(b'0'..=b'9') => {
                    let digits = bytes[at.byte..]
                        .iter()
                        .take(10)
                        .take_while(|ch| ch.is_ascii_digit())
                        .count();
                    if at.byte + digits == bytes.len() && digits <= 9 && !finish {
                        return None;
                    }
                    (digits <= 9 && matches!(bytes.get(at.byte + digits), Some(b'.' | b')')))
                        .then_some(digits + 1)
                }
                _ => None,
            };
            if let Some(marker) = marker {
                let mut content = at;
                content.marker(marker);
                if content.byte == bytes.len() && !finish {
                    return None;
                }
                if matches!(bytes.get(content.byte), Some(b' ' | b'\t')) {
                    let padding = content.indent(bytes, 5);
                    if padding < 5 && content.byte == bytes.len() && !finish {
                        return None;
                    }
                    let width = if padding >= 5 {
                        content = at;
                        content.marker(marker);
                        content.padding_column(bytes);
                        1
                    } else {
                        padding
                    };
                    self.containers
                        .push(Container::List(indent + marker + width));
                    self.at = content;
                    continue;
                }
            }
            return Some(self.prefix(at, false, &self.containers));
        }
    }

    fn continue_containers(
        &mut self,
        text: &str,
        finish: bool,
        containers: &[Container],
    ) -> Option<Context> {
        let bytes = text.as_bytes();
        for (i, container) in containers.iter().enumerate().skip(self.next) {
            let mut at = self.at;
            match container {
                Container::Quote => {
                    if at.indent(bytes, 4) > 3 {
                        return Some(Context::Left);
                    }
                    if at.byte == bytes.len() && !finish {
                        return None;
                    }
                    if !at.quote(bytes, finish)? {
                        return Some(Context::Left);
                    }
                }
                Container::List(width) => {
                    let padding = at.indent(bytes, *width);
                    if padding < *width {
                        if at.byte == bytes.len() && !finish {
                            return None;
                        }
                        if at.byte == bytes.len() || bytes.get(at.byte) == Some(&b'\n') {
                            return Some(Context::Blank(self.prefix(at, false, containers)));
                        }
                        return Some(Context::Left);
                    }
                    at.carry = padding - width;
                }
            }
            self.at = at;
            self.containers.push(container.clone());
            self.next = i + 1;
        }
        Some(Context::Matched)
    }

    /// Only the opening containers are consumed inside a fence. Blank lines
    /// may omit list padding; they do not leave the list or close its fence.
    pub(super) fn fence(
        &mut self,
        text: &str,
        finish: bool,
        containers: &[Container],
    ) -> Option<Option<Prefix>> {
        match self.continue_containers(text, finish, containers)? {
            Context::Left => return Some(None),
            Context::Blank(prefix) => return Some(Some(prefix)),
            Context::Matched => {}
        }
        let mut at = self.at;
        let indent = at.indent(text.as_bytes(), 4);
        if indent < 4 && at.byte == text.len() && !finish {
            return None;
        }
        Some(Some(self.prefix(at, indent >= 4, containers)))
    }
}

enum Context {
    Matched,
    Left,
    Blank(Prefix),
}

/// Use the same CommonMark parser already used by the CLI rather than a second
/// list of paragraph-interruption rules. Called once per complete ambiguous
/// continuation line; the streaming owner retains its newline scan cursor.
pub(super) fn interrupts_paragraph(line: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag};
    const PREFIX: &str = "paragraph\n";
    let source = format!("{PREFIX}{line}");
    !matches!(Parser::new(&source).into_offset_iter().next(),Some((Event::Start(Tag::Paragraph),span)) if span.end>PREFIX.len())
}

/// ATX headings are single-line blocks; unmatched inline delimiters cannot
/// extend into the following paragraph. Eight leading bytes suffice to decide
/// the heading marker (up to six ASCII # characters followed by whitespace).
pub(super) fn line_is_heading(head: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag};
    matches!(
        Parser::new(head).next(),
        Some(Event::Start(Tag::Heading { .. }))
    )
}

/// Only CommonMark block-start candidates need complete-line lookahead.
/// Ordinary prose continues streaming; this is a bounded lexical check, while
/// the actual block decision stays with `interrupts_paragraph`.
pub(super) fn may_interrupt(text: &str) -> bool {
    text.chars()
        .take(4)
        .find(|ch| !matches!(ch, ' ' | '\t'))
        .is_none_or(|ch| {
            matches!(
                ch,
                '#' | '>' | '<' | '`' | '~' | '-' | '_' | '*' | '+' | '0'..='9' | '\r' | '\n'
            )
        })
}
