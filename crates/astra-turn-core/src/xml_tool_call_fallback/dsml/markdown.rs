//! Incremental Markdown line/container prefixes used by the DSML lexer.
//! Cursor checkpoints retain completed containers. Only bounded marker/padding
//! lookahead can be revisited; long indentation is published once it is literal.

use std::sync::Arc;

#[derive(Clone, Debug)]
pub(super) enum Container {
    Quote,
    List(ListPrefix),
}

#[derive(Clone, Debug)]
pub(super) struct ListPrefix {
    indent: usize,
    marker_len: usize,
    delimiter: u8,
    width: usize,
    content: ItemContent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ItemContent {
    MarkerLine,
    AwaitingBlock,
    Started,
}

impl ListPrefix {
    fn write_to(&self, output: &mut String) {
        output.extend(std::iter::repeat_n(' ', self.indent));
        // The value of an ordered marker on the context line doesn't affect
        // sibling membership, but its delimiter and column width do.
        output.extend(std::iter::repeat_n('1', self.marker_len - 1));
        output.push(char::from(self.delimiter));
        output.extend(std::iter::repeat_n(
            ' ',
            self.width - self.indent - self.marker_len,
        ));
    }
}

/// An empty marker may start its first block on the next line. A blank
/// separator before that block closes the item; a populated item can still
/// have blank lines between its blocks. Ancestors are already populated by
/// their child containers, so only the innermost item can be awaiting content.
pub(super) fn finish_container_line(containers: &mut Arc<[Container]>, blank: bool) {
    let Some(Container::List(list)) = containers.last() else {
        return;
    };
    match (list.content, blank) {
        (ItemContent::Started, _) => {}
        (ItemContent::AwaitingBlock, true) => {
            *containers = Arc::from(&containers[..containers.len() - 1]);
        }
        (content, blank) => {
            if let Some(Container::List(list)) = Arc::make_mut(containers).last_mut() {
                list.content = if content == ItemContent::MarkerLine && blank {
                    ItemContent::AwaitingBlock
                } else {
                    ItemContent::Started
                };
            }
        }
    }
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
    fn enter(&mut self, container: Container) {
        if let Some(Container::List(list)) = self.containers.last_mut() {
            // The marker is content of its parent even if it opens an empty
            // block or CommonMark later classifies it as a rule.
            list.content = ItemContent::Started;
        }
        self.containers.push(container);
    }

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
                self.enter(Container::Quote);
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
                let padding = if matches!(bytes.get(content.byte), Some(b' ' | b'\t')) {
                    let padding = content.indent(bytes, 5);
                    if padding >= 5 {
                        content = at;
                        content.marker(marker);
                        content.padding_column(bytes);
                        Some(1)
                    } else if blank_line_start(&bytes[content.byte..], finish)? {
                        Some(1)
                    } else {
                        Some(padding)
                    }
                } else {
                    blank_line_start(&bytes[content.byte..], finish)?.then_some(1)
                };
                if let Some(padding) = padding {
                    self.enter(Container::List(ListPrefix {
                        indent,
                        marker_len: marker,
                        delimiter: bytes[at.byte + marker - 1],
                        width: indent + marker + padding,
                        content: ItemContent::MarkerLine,
                    }));
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
                Container::List(list) => {
                    let padding = at.indent(bytes, list.width);
                    if padding < list.width {
                        if blank_line_start(&bytes[at.byte..], finish)? {
                            return Some(Context::Blank(self.prefix(at, false, containers)));
                        }
                        return Some(Context::Left);
                    }
                    at.carry = padding - list.width;
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

/// Match the existing parser's LF/CRLF physical-line grammar without rewriting
/// source bytes. A trailing CR needs one byte of lookahead across deltas.
fn blank_line_start(bytes: &[u8], finish: bool) -> Option<bool> {
    match bytes {
        [] => finish.then_some(true),
        [b'\r'] if !finish => None,
        [b'\n', ..] | [b'\r', b'\n', ..] => Some(true),
        _ => Some(false),
    }
}

/// Facts from the canonical CommonMark parser for a complete physical line.
/// Paragraph continuations (including indented ones) remain distinct from
/// headings, literal blocks and new nested containers.
pub(super) struct Boundary {
    pub interrupts: bool,
    pub paragraph: bool,
    pub html: Option<HtmlStart>,
    containers: Vec<ContainerKind>,
}

pub(super) enum HtmlStart {
    Element(super::html::BlockEnd),
    Opaque,
}

enum ContainerKind {
    Quote,
    List,
}

impl Prefix {
    /// Extra indentation is decisive only within already established
    /// containers. A new marker can interrupt a paragraph even when the
    /// item itself begins with indented code.
    pub(super) fn settled_indentation(&self) -> bool {
        self.indented && self.inherited == self.containers.len()
    }

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

fn inspect_line(line: &str, previous: Option<&[Container]>, tag: Option<(&str, &str)>) -> Boundary {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut source = String::new();
    if let Some(containers) = previous {
        // Matched containers have already been consumed on both lines. The
        // first unmatched container is the level at which this line can
        // start a sibling. It cannot explicitly continue any deeper container
        // without matching this one; lazy continuation only needs its paragraph.
        if let Some(container) = containers.first() {
            match container {
                Container::Quote => source.push_str("> "),
                Container::List(list) => list.write_to(&mut source),
            }
        }
        source.push_str("paragraph\n");
    }
    let start = source.len();
    source.push_str(line);
    let mut interrupts = previous.is_some();
    let mut paragraph = false;
    let mut containers = Vec::new();
    let mut literal_block = false;
    let mut html_block = false;
    for (event, span) in Parser::new(&source).into_offset_iter() {
        match event {
            Event::Start(Tag::HtmlBlock) => {
                html_block = span.start >= start;
                literal_block = true;
            }
            Event::Start(Tag::Heading { .. } | Tag::CodeBlock(_)) => {
                literal_block = true;
            }
            Event::End(TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::HtmlBlock) => {
                literal_block = false;
            }
            Event::Start(Tag::Paragraph) if span.end > start => {
                paragraph = true;
                if span.start < start {
                    interrupts = false;
                }
            }
            // Tight list items omit Paragraph events. A break joining the
            // synthetic text to this line means the same paragraph continues.
            Event::SoftBreak | Event::HardBreak if span.start < start && span.end == start => {
                interrupts = false;
                paragraph = true;
            }
            // Tight list items omit Paragraph events, but their inline content
            // still owns the same paragraph continuation and delimiter lifetime.
            Event::Text(_) | Event::Code(_) if !literal_block && span.end > start => {
                paragraph = true
            }
            Event::Start(Tag::BlockQuote(_)) if span.start >= start => {
                containers.push(ContainerKind::Quote);
            }
            // A new sibling opens an Item but shares the enclosing List.
            Event::Start(Tag::Item) if span.start >= start => {
                containers.push(ContainerKind::List);
            }
            _ => {}
        }
    }
    Boundary {
        interrupts,
        paragraph,
        html: html_block.then(|| {
            tag.map_or(HtmlStart::Opaque, |(raw, name)| {
                HtmlStart::Element(super::html::BlockEnd::for_tag(raw, name))
            })
        }),
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

/// A backtick in the info string rejects the opener; otherwise the fence
/// isn't known until the physical line ends. Both block interruption and
/// fence entry must give CommonMark the same decisive prefix.
pub(super) fn backtick_fence_prefix(
    text: &str,
    run: usize,
    checked: &mut usize,
    finish: bool,
) -> Option<usize> {
    let start = (*checked).max(run);
    if let Some(index) = text[start..]
        .bytes()
        .position(|byte| matches!(byte, b'\n' | b'`'))
    {
        Some(start + index + 1)
    } else if finish {
        Some(text.len())
    } else {
        *checked = text.len();
        None
    }
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

/// Checkpoint line classification until the leading payload supplies enough
/// CommonMark facts. An HTML opener/closer must be recognized before deciding
/// if a paragraph or a lazy list/quote continuation ends on this line.
#[derive(Debug, Default)]
pub(super) struct LineProbe {
    checked: usize,
    backticks: usize,
    tag: super::tag::Scanner,
    html_tag: Option<(usize, String)>,
    prefix_checked: bool,
}

impl LineProbe {
    pub(super) fn inspect(
        &mut self,
        text: &str,
        prefix: &Prefix,
        previous: Option<&[Container]>,
        finish: bool,
    ) -> Option<Boundary> {
        use super::tag::Scan;
        let start = prefix.end;
        let previous = previous.map(|containers| &containers[prefix.inherited..]);
        if !text[start..].starts_with('<') {
            let payload = &text[start..];
            let end = if payload.starts_with('`') {
                self.backticks += payload.as_bytes()[self.backticks..]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                if self.backticks == payload.len() && !finish {
                    return None;
                }
                if self.backticks >= 3 {
                    start
                        + backtick_fence_prefix(payload, self.backticks, &mut self.checked, finish)?
                } else {
                    start
                        + self.backticks
                        + payload[self.backticks..]
                            .chars()
                            .next()
                            .map_or(0, char::len_utf8)
                }
            } else {
                let end = classification_end(text, self.checked.max(start), finish);
                self.checked = text.len();
                end?
            };
            return Some(inspect_line(
                &text[prefix.continuation_end..end],
                previous,
                None,
            ));
        }
        if self.html_tag.is_none() {
            let scan = self.tag.scan(&text[start..], finish);
            match scan {
                Scan::Html { end, name, .. } => self.html_tag = Some((start + end, name)),
                Scan::Opaque { end, .. } => {
                    return Some(inspect_line(
                        &text[prefix.continuation_end..start + end],
                        previous,
                        None,
                    ));
                }
                _ => {
                    if !self.prefix_checked
                        && let Some((end, name)) = self.tag.html_prefix(&text[start..])
                    {
                        self.prefix_checked = true;
                        let boundary = inspect_line(
                            &text[prefix.continuation_end..start + end],
                            previous,
                            Some((&text[start..start + end], name)),
                        );
                        if boundary.html.is_some() {
                            return Some(boundary);
                        }
                    }
                    // Protocol tags do not start Markdown HTML blocks.
                    return (!matches!(scan, Scan::Pending)).then(|| {
                        inspect_line(&text[prefix.continuation_end..start + 1], previous, None)
                    });
                }
            }
        }
        let (tag_end, name) = self.html_tag.as_ref().unwrap();
        // A complete tag alone can start a type-7 block; trailing
        // inline content changes that classification.
        let checked = self.checked.max(*tag_end);
        let end = if let Some((index, ch)) = text[checked..]
            .char_indices()
            .find(|(_, ch)| *ch == '\n' || !ch.is_whitespace())
        {
            checked + index + ch.len_utf8()
        } else if finish {
            text.len()
        } else {
            self.checked = text.len();
            return None;
        };
        Some(inspect_line(
            &text[prefix.continuation_end..end],
            previous,
            Some((&text[start..*tag_end], name)),
        ))
    }
}

/// Probe a resolved container prefix only through its first inline payload.
/// Ordinary numbered/bulleted/heading text need not wait for newline or EOF.
/// Marker-only input remains ambiguous; its scan cursor is checkpointed.
fn classification_end(text: &str, start: usize, finish: bool) -> Option<usize> {
    text[start..]
        .char_indices()
        .find(|(_, ch)| {
            *ch == '\n'
                || (!ch.is_whitespace() && !matches!(ch, '#' | '*' | '-' | '_' | '=' | '~' | '`'))
        })
        .map(|(at, ch)| start + at + ch.len_utf8())
        .or_else(|| finish.then_some(text.len()))
}
