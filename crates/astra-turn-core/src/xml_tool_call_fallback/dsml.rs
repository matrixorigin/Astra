//! Markup token boundaries shared by call recovery and visible-text projection.
//! Markdown and HTML literals are data; only explicit protocol tags outside them carry
//! tool intent. Raw provider bytes remain available to the existing admission
//! path, including named calls whose arguments cannot be decoded.

use serde_json::{Value, json};
use std::{collections::HashMap, ops::Range, sync::Arc};

mod html;
mod markdown;
mod tag;
use markdown::Container;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Calls,
    Invoke,
    Parameter,
    ToolCall,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Family {
    #[default]
    Bare,
    Dsml,
}

#[derive(Debug)]
struct Tag {
    kind: Kind,
    family: Family,
    recoverable: bool,
    visible: bool,
    closing: bool,
    span: Range<usize>,
    raw: String,
    literal: bool,
    literal_block: usize,
    attributes: Option<tag::Attributes>,
}

enum Fragment {
    Text {
        text: String,
        visible: bool,
        literal: bool,
    },
    Tag(Tag),
    Incomplete,
}

#[derive(Debug, Default)]
struct Lexer {
    pending: String,
    offset: usize,
    cursor: usize,
    line_prefix_done: bool,
    indented: bool,
    line_containers: Arc<[Container]>,
    block_containers: Arc<[Container]>,
    paragraph_open: bool,
    line_paragraph: bool,
    literal_block: usize,
    line_probe: markdown::LineProbe,
    line_opaque_block: bool,
    fence_containers: Arc<[Container]>,
    prefix_cursor: markdown::Cursor,
    tag_scanner: Option<tag::Scanner>,
    prepared_prefix: Option<markdown::Prefix>,
    html: html::Context,
    opaque: Option<Opaque>,
    delimiter_run: usize,
    fence_close_candidate: bool,
    fence_info_checked: usize,
    literal: Option<(char, usize, bool)>,
    inline: Option<InlineProbe>,
    inline_plan: Option<InlinePlan>,
    stream_published: Option<usize>,
    line_has_text: bool,
    escaped: bool,
    open: Scopes,
}

/// An inline opener is provisional until an equal run closes it in its block.
/// Retain a replay checkpoint for bytes whose ownership may still change.
#[derive(Debug)]
struct InlineProbe {
    start: usize,
    offset: usize,
    line_prefix_done: bool,
    line_has_text: bool,
    line_paragraph: bool,
    paragraph_open: bool,
    escaped: bool,
    competing_run: bool,
    line_containers: Arc<[Container]>,
    block_containers: Arc<[Container]>,
}

/// On an unmatched opener, replay the resolved block tail once. Index the
/// last run of each length so that other unmatched runs cannot rescan it.
#[derive(Debug)]
struct InlinePlan {
    end: usize,
    last: HashMap<usize, usize>,
}

#[derive(Debug)]
struct Opaque {
    terminator: &'static str,
    containers: Arc<[Container]>,
    block: bool,
}

#[derive(Debug)]
struct Scope {
    kind: Kind,
    family: Family,
    previous_same: Option<usize>,
    literal: bool,
    hidden: bool,
    body: InvocationBody,
}

/// JSON strings and containers own their bytes before tag recognition, even
/// when the argument decoder later rejects their shape or syntax. Numbers
/// and literal keywords contain no string or nested value to protect.
#[derive(Debug, Default)]
enum InvocationBody {
    #[default]
    Markup,
    Pending,
    Json {
        closes: Vec<u8>,
        string: bool,
        escaped: bool,
    },
}

impl InvocationBody {
    fn json_prefix(&mut self, text: &str, line_bounded: bool) -> usize {
        let start = if matches!(self, Self::Pending) {
            let Some((index, first)) = text
                .char_indices()
                .find(|(_, ch)| !ch.is_whitespace() || (line_bounded && *ch == '\n'))
            else {
                return text.len();
            };
            if line_bounded && first == '\n' {
                return index + 1;
            }
            if !matches!(first, '{' | '[' | '"') {
                *self = Self::Markup;
                return 0;
            }
            *self = Self::Json {
                closes: Vec::new(),
                string: false,
                escaped: false,
            };
            index
        } else {
            0
        };
        let Self::Json {
            closes,
            string,
            escaped,
        } = self
        else {
            return 0;
        };
        for (index, byte) in text.bytes().enumerate().skip(start) {
            if *string {
                if *escaped {
                    *escaped = false;
                } else if byte == b'\\' {
                    *escaped = true;
                } else if byte == b'"' {
                    *string = false;
                    if closes.is_empty() {
                        *self = Self::Markup;
                        return index + 1;
                    }
                }
            } else {
                match byte {
                    b'"' => *string = true,
                    b'{' => closes.push(b'}'),
                    b'[' => closes.push(b']'),
                    b'}' | b']' if closes.last() == Some(&byte) => {
                        closes.pop();
                        if closes.is_empty() {
                            *self = Self::Markup;
                            return index + 1;
                        }
                    }
                    _ => {}
                }
            }
            // Escaped examples belong to a Markdown block. Revisit the
            // next line before granting the example any more JSON ownership.
            if line_bounded && byte == b'\n' {
                return index + 1;
            }
        }
        text.len()
    }
}

/// One indexed stack owns every protocol family before candidate validation.
/// A parameter or invocation is opaque to independent nested recovery.
#[derive(Debug, Default)]
struct Scopes {
    frames: Vec<Scope>,
    last: [Option<usize>; 8],
    blocking: usize,
}
impl Scopes {
    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    fn kind(&self) -> Option<(Family, Kind)> {
        self.frames.last().map(|scope| (scope.family, scope.kind))
    }
    fn visible(&self) -> bool {
        self.frames.last().is_none_or(|scope| !scope.hidden)
    }
    fn literal(&self) -> bool {
        self.frames.last().is_some_and(|scope| scope.literal)
    }
    fn markdown_active(&self) -> bool {
        self.is_empty() || self.literal()
    }
    fn allows_call(&self, family: Family, kind: Kind) -> bool {
        self.blocking == 0 && (self.is_empty() || (family == Family::Dsml && kind == Kind::Invoke))
    }
    fn push(&mut self, family: Family, kind: Kind, literal: bool) {
        let key = family as usize * 4 + kind as usize;
        let previous_same = self.last[key];
        self.last[key] = Some(self.frames.len());
        let hidden = !literal && (family == Family::Dsml || !self.visible());
        if kind != Kind::Calls {
            self.blocking += 1;
        }
        self.frames.push(Scope {
            kind,
            family,
            previous_same,
            literal,
            hidden,
            body: if kind == Kind::Invoke {
                InvocationBody::Pending
            } else {
                InvocationBody::Markup
            },
        });
    }
    fn clear(&mut self) {
        self.frames.clear();
        self.last = [None; 8];
        self.blocking = 0;
    }
    fn close(&mut self, family: Family, kind: Kind) -> Option<bool> {
        let index = self.last[family as usize * 4 + kind as usize]?;
        let literal = self.frames[index].literal;
        while self.frames.len() > index {
            let scope = self.frames.pop().unwrap();
            self.last[scope.family as usize * 4 + scope.kind as usize] = scope.previous_same;
            if scope.kind != Kind::Calls {
                self.blocking -= 1;
            }
        }
        Some(literal)
    }
}

impl Lexer {
    fn remaining(&self) -> &str {
        &self.pending[self.cursor..]
    }

    fn projection_text(&self, start: usize, end: usize) -> String {
        // A streaming callback may have received the common prefix before an
        // inline match or replay. Final projection has no published watermark.
        let published = self
            .stream_published
            .map_or(0, |offset| offset.saturating_sub(self.offset - self.cursor));
        self.pending[published.max(start).min(end)..end].to_owned()
    }

    fn inline_checkpoint(&self) -> InlineProbe {
        InlineProbe {
            start: self.cursor,
            offset: self.offset,
            line_prefix_done: self.line_prefix_done,
            line_has_text: self.line_has_text,
            line_paragraph: self.line_paragraph,
            paragraph_open: self.paragraph_open,
            escaped: self.escaped,
            competing_run: false,
            line_containers: self.line_containers.clone(),
            block_containers: self.block_containers.clone(),
        }
    }

    fn publish_inline_prose(&mut self, fragments: &mut Vec<Fragment>) {
        let (Some(published), Some(inline)) = (self.stream_published, &self.inline) else {
            return;
        };
        let base = self.offset - self.cursor;
        let start = inline.start.max(published.saturating_sub(base));
        let end = self.pending[start..self.cursor]
            .find('<')
            .map_or(self.cursor, |index| start + index);
        if end > start {
            fragments.push(Fragment::Text {
                text: self.pending[start..end].to_owned(),
                visible: true,
                literal: true,
            });
            self.stream_published = Some(base + end);
        }
        if end == self.cursor && !inline.competing_run {
            // In either interpretation this prefix is just text. A different
            // run could open a span on replay, so keep it and its raw suffix.
            let mut checkpoint = self.inline_checkpoint();
            checkpoint.paragraph_open |= checkpoint.line_paragraph;
            self.inline = Some(checkpoint);
        }
    }

    fn push(&mut self, chunk: &str, finish: bool) -> Vec<Fragment> {
        self.pending.push_str(chunk);
        let mut fragments = Vec::new();
        loop {
            if self.inline.is_some()
                && (self.literal.is_none() || (finish && self.remaining().is_empty()))
            {
                self.replay_inline();
            }
            if self.remaining().is_empty() {
                break;
            }
            if self
                .inline_plan
                .as_ref()
                .is_some_and(|plan| self.offset >= plan.end)
            {
                self.inline_plan = None;
            }
            if let Some(opaque) = &self.opaque {
                if !self.line_prefix_done {
                    let Some(prefix) = self.prefix_cursor.fence(
                        &self.pending[self.cursor..],
                        finish,
                        &opaque.containers,
                    ) else {
                        break;
                    };
                    self.prefix_cursor = markdown::Cursor::default();
                    if let Some(prefix) = prefix {
                        let text = self.projection_text(self.cursor, self.cursor + prefix.end);
                        self.consume(prefix.end);
                        fragments.push(Fragment::Text {
                            text,
                            visible: true,
                            literal: true,
                        });
                        self.line_prefix_done = true;
                    } else {
                        self.opaque = None;
                        self.html.clear();
                        continue;
                    }
                }
                let terminator = self.opaque.as_ref().unwrap().terminator;
                let line_end = self.remaining().find('\n').map(|at| at + 1);
                let limit = line_end.unwrap_or(self.remaining().len());
                let end = if let Some(at) = self.remaining()[..limit].find(terminator) {
                    let opaque = self.opaque.take().unwrap();
                    if opaque.block {
                        self.html
                            .start_block(html::BlockEnd::Line, opaque.containers);
                    }
                    at + terminator.len()
                } else if finish || line_end.is_some() {
                    limit
                } else {
                    let mut end = limit.saturating_sub(terminator.len() - 1);
                    while !self.remaining().is_char_boundary(end) {
                        end -= 1;
                    }
                    end
                };
                if end == 0 {
                    break;
                }
                let text = self.projection_text(self.cursor, self.cursor + end);
                self.consume(end);
                fragments.push(Fragment::Text {
                    text,
                    visible: true,
                    literal: true,
                });
                continue;
            }
            if !self.line_prefix_done {
                let inline = matches!(self.literal, Some((_, _, false)));
                let escaped_scope = self.open.literal();
                let contextual = inline || escaped_scope || self.paragraph_open;
                if self.open.markdown_active() {
                    let mut prefix = self.prepared_prefix.take();
                    if prefix.is_none() && self.html.containers().is_some() {
                        let Some(html_prefix) = self.prefix_cursor.fence(
                            &self.pending[self.cursor..],
                            finish,
                            self.html.containers().unwrap(),
                        ) else {
                            break;
                        };
                        if html_prefix.is_none() {
                            self.html.clear();
                            self.prefix_cursor = markdown::Cursor::default();
                        }
                        prefix = html_prefix;
                    }
                    if prefix.is_none() && matches!(self.literal, Some((_, _, true))) {
                        let Some(fence_prefix) = self.prefix_cursor.fence(
                            &self.pending[self.cursor..],
                            finish,
                            &self.fence_containers,
                        ) else {
                            break;
                        };
                        if fence_prefix.is_none() {
                            self.literal = None;
                            self.prefix_cursor = markdown::Cursor::default();
                        }
                        prefix = fence_prefix;
                    }
                    let Some(mut prefix) = prefix.or_else(|| {
                        self.prefix_cursor.normal(
                            &self.pending[self.cursor..],
                            finish,
                            &self.block_containers,
                        )
                    }) else {
                        break;
                    };
                    self.prefix_cursor = markdown::Cursor::default();
                    // Leaving a literal container changes which grammar owns
                    // this same line, before deriving its next block facts.
                    let classify = !self.html.active()
                        && self.literal.is_none_or(|(_, _, fence)| !fence)
                        && markdown::may_interrupt(self.remaining());
                    let boundary = if classify {
                        if let Some(boundary) = self.line_probe.inspect(
                            &self.pending[self.cursor..],
                            &prefix,
                            contextual,
                            finish,
                        ) {
                            Some(boundary)
                        } else if prefix.indented {
                            // Indentation is already decidable; publish whitespace
                            // while preserving a continuing paragraph until newline.
                            None
                        } else {
                            self.prepared_prefix = Some(prefix);
                            break;
                        }
                    } else {
                        None
                    };
                    let interrupts = boundary
                        .as_ref()
                        .is_some_and(|boundary| boundary.interrupts);
                    if inline && interrupts {
                        if self.inline.is_some() {
                            self.replay_inline();
                            continue;
                        }
                        self.literal = None;
                    }
                    if escaped_scope && interrupts {
                        self.open.clear();
                        self.literal_block += 1;
                    }
                    let continuation = self.paragraph_open && !interrupts;
                    self.line_paragraph = boundary
                        .as_ref()
                        .map(|boundary| boundary.paragraph)
                        .unwrap_or(!prefix.indented || continuation || inline)
                        && self.html.containers().is_none();
                    if let Some(boundary) = &boundary {
                        prefix.validate_containers(boundary);
                    }
                    if interrupts {
                        self.paragraph_open = false;
                    }
                    self.indented = prefix.indented
                        && !continuation
                        && !matches!(self.literal, Some((_, _, false)));
                    self.line_containers = if contextual && prefix.left_context && !interrupts {
                        self.block_containers.clone()
                    } else {
                        prefix.containers.into()
                    };
                    self.block_containers = self.line_containers.clone();
                    self.line_opaque_block = false;
                    match boundary.and_then(|boundary| boundary.html) {
                        Some(markdown::HtmlStart::Element(end)) => {
                            self.html.start_block(end, self.line_containers.clone());
                        }
                        Some(markdown::HtmlStart::Opaque) => self.line_opaque_block = true,
                        None => {}
                    }
                    let end = prefix.end;
                    if end > 0 {
                        let text = self
                            .inline
                            .is_none()
                            .then(|| self.projection_text(self.cursor, self.cursor + end));
                        self.consume(end);
                        self.line_has_text = false;
                        if let Some(text) = text {
                            fragments.push(Fragment::Text {
                                text,
                                visible: true,
                                literal: true,
                            });
                        }
                    }
                }
                self.line_prefix_done = true;
            }
            let first = self.remaining().chars().next().unwrap_or('\n');
            if self.remaining().is_empty() {
                break;
            }
            if let Some(end) = self.html.block_prefix(&self.pending[self.cursor..], finish) {
                if end == 0 {
                    break;
                }
                let text = self.projection_text(self.cursor, self.cursor + end);
                self.consume(end);
                fragments.push(Fragment::Text {
                    text,
                    visible: true,
                    literal: true,
                });
                continue;
            }
            // The closing fence line is literal until its remainder is known.
            // Publish whitespace incrementally instead of rescanning a growing
            // tail while waiting to decide whether the fence really closes.
            if self.fence_close_candidate {
                if first == '\n' {
                    self.literal = None;
                } else if matches!(first, ' ' | '\t' | '\r') {
                    let end = self
                        .remaining()
                        .bytes()
                        .take_while(|ch| matches!(ch, b' ' | b'\t' | b'\r'))
                        .count();
                    let text = self.projection_text(self.cursor, self.cursor + end);
                    self.consume(end);
                    fragments.push(Fragment::Text {
                        text,
                        visible: true,
                        literal: true,
                    });
                    continue;
                }
                self.fence_close_candidate = false;
            }
            let json = self.open.frames.last_mut().map_or(0, |scope| {
                scope
                    .body
                    .json_prefix(&self.pending[self.cursor..], scope.literal)
            });
            if json != 0 {
                let text = self.projection_text(self.cursor, self.cursor + json);
                let visible = self.open.visible();
                let literal = self.open.literal();
                self.consume(json);
                fragments.push(Fragment::Text {
                    text,
                    visible,
                    literal,
                });
                continue;
            }
            if self.literal.is_none() && !self.indented && first == '<' {
                let mut scanner = self.tag_scanner.take().unwrap_or_default();
                match scanner.scan(self.remaining(), finish) {
                    tag::Scan::Pending => {
                        self.tag_scanner = Some(scanner);
                        break;
                    }
                    tag::Scan::Incomplete => {
                        let len = self.remaining().len();
                        if self.open.visible() && (self.escaped || !self.open.is_empty()) {
                            let text = self.projection_text(self.cursor, self.pending.len());
                            self.consume(len);
                            fragments.push(Fragment::Text {
                                text,
                                visible: true,
                                literal: true,
                            });
                        } else {
                            self.consume(len);
                            fragments.push(Fragment::Incomplete);
                        }
                        continue;
                    }
                    tag::Scan::Opaque { end, terminator }
                        if self.open.is_empty() && !self.escaped =>
                    {
                        let text = self.projection_text(self.cursor, self.cursor + end);
                        if !self.line_has_text {
                            self.line_paragraph = false;
                            self.paragraph_open = false;
                        }
                        self.consume(end);
                        self.opaque = Some(Opaque {
                            terminator,
                            containers: self.line_containers.clone(),
                            block: self.line_opaque_block,
                        });
                        fragments.push(Fragment::Text {
                            text,
                            visible: true,
                            literal: true,
                        });
                        continue;
                    }
                    tag::Scan::Html { end, name, closing } => {
                        let text = self.projection_text(self.cursor, self.cursor + end);
                        if self.open.is_empty() && !self.escaped {
                            self.html.tag(&text, &name, closing);
                        }
                        let visible = self.open.visible();
                        self.consume(end);
                        fragments.push(Fragment::Text {
                            text,
                            visible,
                            literal: true,
                        });
                        continue;
                    }
                    tag::Scan::Tag {
                        end,
                        kind,
                        family,
                        closing,
                        attributes,
                    } if self.html.active()
                        || !matches!(self.open.kind(), Some((_, Kind::Parameter)))
                        || (closing && self.open.kind() == Some((family, kind))) =>
                    {
                        let raw = self.remaining()[..end].to_owned();
                        let span = self.offset..self.offset + end;
                        let escaped = self.escaped;
                        let literal_block = self.literal_block;
                        let mut literal = self.html.active()
                            || self.open.literal()
                            || (self.open.is_empty() && escaped);
                        let recoverable =
                            !literal && !closing && self.open.allows_call(family, kind);
                        let visible = literal || (family == Family::Bare && self.open.visible());
                        if !self.html.active() {
                            if closing && !escaped {
                                literal |= self.open.close(family, kind).unwrap_or(false);
                            } else if !closing
                                && !raw.trim_end_matches('>').trim_end().ends_with('/')
                            {
                                self.open.push(family, kind, literal);
                            }
                        }
                        self.consume(end);
                        fragments.push(Fragment::Tag(Tag {
                            kind,
                            family,
                            recoverable,
                            visible: visible || literal,
                            closing,
                            span,
                            raw,
                            literal,
                            literal_block,
                            attributes,
                        }));
                        continue;
                    }
                    tag::Scan::Ordinary | tag::Scan::Tag { .. } | tag::Scan::Opaque { .. } => {}
                }
            }
            if self.open.is_empty()
                && !self.html.active()
                && !self.indented
                && matches!(first, '`' | '~')
                && (!self.escaped || self.literal.is_some())
            {
                let count = self.delimiter_run
                    + self.remaining()[self.delimiter_run..]
                        .bytes()
                        .take_while(|ch| *ch == first as u8)
                        .count();
                if count == self.remaining().len() && !finish {
                    self.delimiter_run = count;
                    break;
                }
                self.delimiter_run = 0;
                let fence_end = matches!(self.literal,Some((delimiter,length,true)) if first == delimiter && !self.line_has_text && count >= length);
                let mut text_start = self.cursor;
                match self.literal {
                    Some((delimiter, length, fence))
                        if first == delimiter
                            && ((!fence && count == length) || (fence && fence_end)) =>
                    {
                        if fence_end {
                            self.fence_close_candidate = true;
                        } else {
                            self.literal = None;
                            if let Some(inline) = self.inline.take() {
                                text_start = inline.start;
                            }
                        }
                    }
                    None => {
                        let fence = if !self.line_has_text && count >= 3 {
                            let Some(fence) = self.opens_fence(first, count, finish) else {
                                self.delimiter_run = count;
                                break;
                            };
                            fence
                        } else {
                            false
                        };
                        if fence {
                            self.literal = Some((first, count, true));
                            self.fence_containers = self.line_containers.clone();
                            self.paragraph_open = false;
                        } else if first == '`' {
                            self.open_inline(count);
                        }
                    }
                    Some((_, _, false)) if first == '`' => {
                        if let Some(inline) = &mut self.inline {
                            inline.competing_run = true;
                        }
                    }
                    _ => {}
                }
                let text = self
                    .inline
                    .is_none()
                    .then(|| self.projection_text(text_start, self.cursor + count));
                self.consume(count);
                if let Some(text) = text {
                    fragments.push(Fragment::Text {
                        text,
                        visible: true,
                        literal: true,
                    });
                }
                continue;
            }
            // Scan each run once and compact only after the batch. In particular,
            // an ordinary multiline completion must not move its entire suffix
            // for every line. Newlines end a run so prefixes are checked anew.
            let end = if first == '\n' {
                1
            } else {
                self.remaining()
                    .char_indices()
                    .skip(1)
                    .find(|(_, ch)| matches!(ch, '<' | '`' | '~' | '\n'))
                    .map(|(index, _)| index)
                    .unwrap_or(self.remaining().len())
            };
            let text = self
                .inline
                .is_none()
                .then(|| self.projection_text(self.cursor, self.cursor + end));
            let literal = self.literal.is_some()
                || self.indented
                || self.html.active()
                || self.open.literal();
            self.consume(end);
            if let Some(text) = text {
                fragments.push(Fragment::Text {
                    text,
                    visible: self.open.visible(),
                    literal,
                });
            }
        }
        self.publish_inline_prose(&mut fragments);
        let consumed = self
            .inline
            .as_ref()
            .map_or(self.cursor, |inline| inline.start);
        if consumed != 0 {
            self.pending.drain(..consumed);
            self.cursor -= consumed;
            if let Some(inline) = &mut self.inline {
                inline.start -= consumed;
            }
        }
        fragments
    }

    fn opens_fence(&mut self, delimiter: char, count: usize, finish: bool) -> Option<bool> {
        // Only a backtick can disqualify a backtick fence's info string.
        // Tilde openings need no provisional line buffer.
        if delimiter == '~' {
            return Some(markdown::opens_fence(&self.remaining()[..count]));
        }
        let end = markdown::backtick_fence_prefix(
            &self.pending[self.cursor..],
            count,
            &mut self.fence_info_checked,
            finish,
        )?;
        self.fence_info_checked = 0;
        Some(markdown::opens_fence(&self.remaining()[..end]))
    }

    fn open_inline(&mut self, count: usize) {
        if let Some(plan) = &self.inline_plan {
            if plan
                .last
                .get(&count)
                .is_some_and(|last| *last > self.offset)
            {
                self.literal = Some(('`', count, false));
            }
            return;
        }
        self.inline = Some(self.inline_checkpoint());
        self.literal = Some(('`', count, false));
    }

    fn replay_inline(&mut self) {
        let inline = self.inline.take().unwrap();
        let mut last = HashMap::new();
        let mut index = inline.start;
        while index < self.cursor {
            if self.pending.as_bytes()[index] == b'`' {
                let len = self.pending.as_bytes()[index..self.cursor]
                    .iter()
                    .take_while(|byte| **byte == b'`')
                    .count();
                last.insert(len, inline.offset + index - inline.start);
                index += len;
            } else {
                index += 1;
            }
        }
        self.inline_plan = Some(InlinePlan {
            end: self.offset,
            last,
        });
        self.cursor = inline.start;
        self.offset = inline.offset;
        self.line_has_text = inline.line_has_text;
        self.line_paragraph = inline.line_paragraph;
        self.paragraph_open = inline.paragraph_open;
        self.line_containers = inline.line_containers;
        self.block_containers = inline.block_containers;
        self.line_prefix_done = inline.line_prefix_done;
        self.line_opaque_block = false;
        self.indented = false;
        self.escaped = inline.escaped;
        self.literal = None;
        self.delimiter_run = 0;
        self.fence_info_checked = 0;
        self.prefix_cursor = markdown::Cursor::default();
        self.prepared_prefix = None;
        self.line_probe = markdown::LineProbe::default();
    }

    fn consume(&mut self, end: usize) {
        for ch in self.pending[self.cursor..self.cursor + end].chars() {
            if ch == '\n' {
                let blank = !self.line_has_text;
                self.html.newline(blank);
                if blank || !self.line_paragraph {
                    if matches!(self.literal, Some((_, _, false))) {
                        self.literal = None;
                    }
                    if self.open.literal() {
                        self.open.clear();
                        self.literal_block += 1;
                    }
                }
                self.line_has_text = false;
                self.line_prefix_done = false;
                self.indented = false;
                self.prefix_cursor = markdown::Cursor::default();
                self.line_probe = markdown::LineProbe::default();
                self.line_opaque_block = false;
                if blank || !self.line_paragraph {
                    self.paragraph_open = false;
                }
            } else {
                if !ch.is_whitespace() {
                    self.line_has_text = true;
                    self.line_prefix_done = true;
                    if self.line_paragraph
                        && !self.indented
                        && self.literal.is_none()
                        && self.open.is_empty()
                        && !self.html.active()
                        && self.opaque.is_none()
                    {
                        self.paragraph_open = true;
                    }
                }
            }
            self.escaped = ch == '\\' && !self.escaped;
        }
        self.cursor += end;
        self.offset += end;
    }
}

/// Streaming projection and final projection use the exact same lexer.
#[derive(Debug)]
pub struct DsmlToolCallStreamFilter {
    lexer: Lexer,
}

impl Default for DsmlToolCallStreamFilter {
    fn default() -> Self {
        Self {
            lexer: Lexer {
                stream_published: Some(0),
                ..Lexer::default()
            },
        }
    }
}

impl DsmlToolCallStreamFilter {
    pub fn push(&mut self, chunk: &str) -> String {
        self.project(chunk, false)
    }
    pub fn finish(&mut self) -> String {
        self.project("", true)
    }

    fn project(&mut self, chunk: &str, finish: bool) -> String {
        project_fragments(self.lexer.push(chunk, finish)).text
    }
}

pub(super) struct Projection {
    pub text: String,
    pub literal: Vec<Range<usize>>,
}

fn project_fragments(fragments: Vec<Fragment>) -> Projection {
    let mut output = Projection {
        text: String::new(),
        literal: Vec::new(),
    };
    for fragment in fragments {
        let (text, literal) = match fragment {
            Fragment::Text {
                text,
                visible: true,
                literal,
            } => (text, literal),
            Fragment::Tag(tag) if tag.visible => (tag.raw, tag.literal),
            _ => continue,
        };
        let start = output.text.len();
        output.text.push_str(&text);
        if literal && start < output.text.len() {
            if let Some(last) = output.literal.last_mut()
                && last.end == start
            {
                last.end = output.text.len();
            } else {
                output.literal.push(start..output.text.len());
            }
        }
    }
    output
}

pub(super) fn project_complete(text: &str) -> Projection {
    project_fragments(Lexer::default().push(text, true))
}

/// The same ordered token stream owns DSML, bare XML and tool_call nesting.
/// Name/argument validity never releases an inner candidate from its owner.
#[derive(Default)]
pub(super) struct Decoded {
    pub calls: Vec<super::ParsedDegradedCall>,
}

pub(super) fn decode(text: &str) -> Decoded {
    let tags = Lexer::default()
        .push(text, true)
        .into_iter()
        .filter_map(|fragment| {
            if let Fragment::Tag(tag) = fragment {
                Some(tag)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut decoded = Decoded::default();
    let mut index = 0;
    while index < tags.len() {
        let opener = &tags[index];
        if !opener.recoverable || !matches!(opener.kind, Kind::Invoke | Kind::ToolCall) {
            index += 1;
            continue;
        }
        let self_closed = opener.raw.trim_end_matches('>').trim_end().ends_with('/');
        let mut open = Scopes::default();
        open.push(opener.family, opener.kind, false);
        let boundary = if self_closed {
            None
        } else {
            tags[index + 1..]
                .iter()
                .position(|tag| {
                    if tag.literal_block != opener.literal_block {
                        return true;
                    }
                    if tag.closing {
                        if open.close(tag.family, tag.kind).is_some() {
                            return open.is_empty();
                        }
                        return tag.kind == Kind::Calls;
                    }
                    if !tag.raw.trim_end_matches('>').trim_end().ends_with('/') {
                        open.push(tag.family, tag.kind, tag.literal);
                    }
                    false
                })
                .map(|offset| index + 1 + offset)
        };
        let closer = boundary
            .map(|i| &tags[i])
            .filter(|tag| tag.family == opener.family && tag.kind == opener.kind && tag.closing);
        let body_end = if self_closed {
            opener.span.end
        } else {
            boundary.map(|i| tags[i].span.start).unwrap_or(text.len())
        };
        let span_end = closer.map(|tag| tag.span.end).unwrap_or(body_end);
        let call = if opener.kind == Kind::ToolCall {
            super::parse_single_tool_call_tag(
                text[opener.span.end..body_end].replace('\0', "").trim(),
            )
        } else if opener.family == Family::Bare && !self_closed && closer.is_none() {
            None
        } else {
            opener
                .attributes
                .as_ref()
                .and_then(|attributes| attributes.name.as_ref())
                .filter(|name| !name.is_empty())
                .map(|name| {
                    let parameters = tags[index + 1..]
                        .iter()
                        .take_while(|tag| tag.span.start < body_end)
                        .collect::<Vec<_>>();
                    let arguments = if self_closed || closer.is_some() {
                        arguments(
                            &text[opener.span.end..body_end],
                            opener.span.end,
                            &parameters,
                        )
                    } else {
                        None
                    };
                    let arguments = arguments
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| text[opener.span.start..span_end].to_owned());
                    json!({
                        "id": format!("xmlfb_{}", Uuid::new_v4()),
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    })
                })
        };
        if let Some(call) = call {
            decoded.calls.push(super::ParsedDegradedCall {
                span: opener.span.start..span_end,
                call,
                explicit_dsml: opener.family == Family::Dsml,
                format: if opener.kind == Kind::ToolCall {
                    super::DegradedCallFormat::ToolCallTag
                } else {
                    super::DegradedCallFormat::Invoke
                },
            });
        }
        index = if self_closed {
            index + 1
        } else {
            boundary
                .map(|i| if closer.is_some() { i + 1 } else { i })
                .unwrap_or(tags.len())
        };
    }
    decoded
}

fn arguments(body: &str, base: usize, tags: &[&Tag]) -> Option<Value> {
    if tags.is_empty() {
        return if body.trim().is_empty() {
            Some(json!({}))
        } else {
            serde_json::from_str::<Value>(body)
                .ok()
                .filter(Value::is_object)
        };
    }
    let mut args = serde_json::Map::new();
    let mut cursor = 0;
    let mut index = 0;
    while index < tags.len() {
        let tag = tags[index];
        if tag.kind != Kind::Parameter || tag.closing {
            return None;
        }
        let close = *tags.get(index + 1)?;
        if close.kind != Kind::Parameter || !close.closing {
            return None;
        }
        let attributes = tag.attributes.as_ref()?;
        let name = attributes.name.as_ref()?;
        if args.contains_key(name) {
            return None;
        }
        if !body.get(cursor..tag.span.start - base)?.trim().is_empty() {
            return None;
        }
        let value = body.get(tag.span.end - base..close.span.start - base)?;
        let value = match (tag.family, attributes.string.as_deref()) {
            (Family::Bare, _) => Value::String(value.to_owned()),
            (_, Some("false")) => serde_json::from_str(value).ok()?,
            (_, Some("true") | None) => Value::String(value.to_owned()),
            _ => return None,
        };
        args.insert(name.to_owned(), value);
        cursor = close.span.end - base;
        index += 2;
    }
    body.get(cursor..)?
        .trim()
        .is_empty()
        .then_some(Value::Object(args))
}
