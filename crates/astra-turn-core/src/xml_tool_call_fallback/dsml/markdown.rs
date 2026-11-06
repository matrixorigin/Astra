//! Incremental Markdown line/container prefixes used by the DSML lexer.
//! Cursor checkpoints retain completed containers. Only bounded marker/padding
//! lookahead can be revisited; long indentation is published once it is literal.

#[derive(Clone, Debug)]
pub(super) enum Container {
    Quote,
    List(usize),
}

#[derive(Debug)]
pub(super) struct Prefix {
    pub end: usize,
    pub indented: bool,
    pub containers: Vec<Container>,
    pub continuation_end: usize,
    pub left_context: bool,
    inherited: usize,
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
    inherited: usize,
}

impl Cursor {
    fn prefix(&self, at: Position, indented: bool, containers: &[Container]) -> Prefix {
        Prefix {
            end: at.byte,
            indented,
            containers: containers.to_vec(),
            continuation_end: self.continuation_end,
            left_context: self.left_context,
            inherited: self.inherited,
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
                    prefix.inherited = prefix.containers.len();
                    return Some(prefix);
                }
                Context::Left => {
                    self.left_context = true;
                }
                Context::Matched => {}
            }
            self.inherited_done = true;
            self.continuation_end = self.at.byte;
            self.inherited = self.containers.len();
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

/// Facts from the canonical CommonMark parser for a complete physical line.
/// Paragraph continuations (including indented ones) remain distinct from
/// headings, literal blocks and new nested containers.
pub(super) struct Boundary {
    pub interrupts: bool,
    pub paragraph: bool,
    containers: Vec<ContainerKind>,
}

enum ContainerKind {
    Quote,
    List,
}

impl Prefix {
    pub(super) fn validate_containers(&mut self, boundary: &Boundary) {
        let mut end = self.inherited;
        for (container, kind) in self.containers[self.inherited..]
            .iter()
            .zip(&boundary.containers)
        {
            if matches!(
                (container, kind),
                (Container::Quote, ContainerKind::Quote)
                    | (Container::List(_), ContainerKind::List)
            ) {
                end += 1;
            } else {
                break;
            }
        }
        self.containers.truncate(end);
    }
}

pub(super) fn inspect_line(line: &str, previous_paragraph: bool) -> Boundary {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let prefix = if previous_paragraph {
        "paragraph\n"
    } else {
        ""
    };
    let source = format!("{prefix}{line}");
    let mut events = Parser::new(&source).into_offset_iter().peekable();
    let interrupts = previous_paragraph
        && !matches!(events.peek(),Some((Event::Start(Tag::Paragraph),span)) if span.end>prefix.len());
    let mut paragraph = false;
    let mut containers = Vec::new();
    let mut literal_block = false;
    for (event, span) in events {
        match event {
            Event::Start(Tag::Heading { .. } | Tag::CodeBlock(_) | Tag::HtmlBlock) => {
                literal_block = true;
            }
            Event::End(TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::HtmlBlock) => {
                literal_block = false;
            }
            Event::Start(Tag::Paragraph) if span.end > prefix.len() => paragraph = true,
            // Tight list items omit Paragraph events, but their inline content
            // still owns the same paragraph continuation and delimiter lifetime.
            Event::Text(_) | Event::Code(_) if !literal_block && span.end > prefix.len() => {
                paragraph = true
            }
            Event::Start(Tag::BlockQuote(_)) if span.start >= prefix.len() => {
                containers.push(ContainerKind::Quote);
            }
            Event::Start(Tag::List(_)) if span.start >= prefix.len() => {
                containers.push(ContainerKind::List);
            }
            _ => {}
        }
    }
    Boundary {
        interrupts,
        paragraph,
        containers,
    }
}

/// The opening run alone is provisional. The info string can turn a backtick
/// fence into inline text; use the same CommonMark grammar for both decisions.
pub(super) fn opens_fence(line: &str) -> bool {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};
    matches!(
        Parser::new(line).next(),
        Some(Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))))
    )
}

/// Every punctuation/digit/whitespace prefix is a candidate, rather than a
/// second enumeration of Markdown markers. Only safe leading letters bypass
/// structural-prefix classification; the actual decision remains CommonMark's.
pub(super) fn may_interrupt(text: &str) -> bool {
    text.chars()
        .take(4)
        .find(|ch| !matches!(ch, ' ' | '\t'))
        .is_none_or(|ch| !ch.is_alphabetic())
}

/// Probe a resolved container prefix only through its first inline payload.
/// Ordinary numbered/bulleted/heading text need not wait for newline or EOF.
/// Marker-only input remains ambiguous; its scan cursor is checkpointed.
pub(super) fn classification_end(text: &str, start: usize, finish: bool) -> Option<usize> {
    text[start..]
        .char_indices()
        .find(|(_, ch)| {
            *ch == '\n'
                || (!ch.is_whitespace() && !matches!(ch, '#' | '*' | '-' | '_' | '=' | '~' | '`'))
        })
        .map(|(at, ch)| start + at + ch.len_utf8())
        .or_else(|| finish.then_some(text.len()))
}
