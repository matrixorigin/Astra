//! DSML token boundaries shared by call recovery and visible-text projection.
//! Markdown literals are data; only explicit protocol tags outside them carry
//! tool intent. Raw provider bytes remain available to the existing admission
//! path, including named calls whose arguments cannot be decoded.

use serde_json::{Value, json};
use std::{ops::Range, sync::Arc};

mod markdown;
mod tag;
use markdown::Container;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Calls,
    Invoke,
    Parameter,
}

#[derive(Debug)]
struct Tag {
    kind: Kind,
    closing: bool,
    span: Range<usize>,
    raw: String,
    literal: bool,
    literal_block: usize,
}

enum Fragment {
    Text { text: String, visible: bool },
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
    continuation_checked: usize,
    fence_containers: Arc<[Container]>,
    prefix_cursor: markdown::Cursor,
    tag_scanner: Option<tag::Scanner>,
    delimiter_run: usize,
    fence_close_candidate: bool,
    literal: Option<(char, usize, bool)>,
    line_has_text: bool,
    escaped: bool,
    open: Scopes,
}

#[derive(Debug)]
struct Scope {
    kind: Kind,
    previous_same: Option<usize>,
    literal: bool,
}

/// Each push/pop updates a per-kind index. Missing closers take constant time;
/// matching a parent pops each intervening scope at most once.
#[derive(Debug, Default)]
struct Scopes {
    frames: Vec<Scope>,
    last: [Option<usize>; 3],
}

impl Scopes {
    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    fn kind(&self) -> Option<Kind> {
        self.frames.last().map(|scope| scope.kind)
    }
    fn visible(&self) -> bool {
        self.frames.last().is_none_or(|scope| scope.literal)
    }
    fn push(&mut self, kind: Kind, literal: bool) {
        let previous_same = self.last[kind as usize];
        self.last[kind as usize] = Some(self.frames.len());
        self.frames.push(Scope {
            kind,
            previous_same,
            literal,
        });
    }
    fn clear(&mut self) {
        self.frames.clear();
        self.last = [None; 3];
    }
    fn close(&mut self, kind: Kind) -> Option<bool> {
        let index = self.last[kind as usize]?;
        let literal = self.frames[index].literal;
        while self.frames.len() > index {
            let scope = self.frames.pop().unwrap();
            self.last[scope.kind as usize] = scope.previous_same;
        }
        Some(literal)
    }
}

impl Lexer {
    fn remaining(&self) -> &str {
        &self.pending[self.cursor..]
    }

    fn push(&mut self, chunk: &str, finish: bool) -> Vec<Fragment> {
        self.pending.push_str(chunk);
        let mut fragments = Vec::new();
        while !self.remaining().is_empty() {
            if !self.line_prefix_done {
                let inline = matches!(self.literal, Some((_, _, false)));
                let escaped_scope = !self.open.is_empty() && self.open.visible();
                let contextual = inline || escaped_scope || self.paragraph_open;
                let classify = self.open.visible()
                    && self.literal.is_none_or(|(_, _, fence)| !fence)
                    && markdown::may_interrupt(self.remaining());
                let line_end = if classify {
                    let tail = &self.remaining()[self.continuation_checked..];
                    if let Some(end) = tail.find('\n') {
                        self.continuation_checked + end + 1
                    } else if finish {
                        self.remaining().len()
                    } else {
                        self.continuation_checked = self.remaining().len();
                        break;
                    }
                } else {
                    0
                };
                if self.open.visible() {
                    let prefix = if matches!(self.literal, Some((_, _, true))) {
                        let Some(prefix) = self.prefix_cursor.fence(
                            &self.pending[self.cursor..],
                            finish,
                            &self.fence_containers,
                        ) else {
                            break;
                        };
                        if prefix.is_none() {
                            self.literal = None;
                            self.prefix_cursor = markdown::Cursor::default();
                        }
                        prefix
                    } else {
                        None
                    };
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
                    let boundary = classify.then(|| {
                        markdown::inspect_line(
                            &self.remaining()[prefix.continuation_end..line_end],
                            contextual,
                        )
                    });
                    let interrupts = boundary
                        .as_ref()
                        .is_some_and(|boundary| boundary.interrupts);
                    if inline && interrupts {
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
                        .unwrap_or(!prefix.indented || continuation || inline);
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
                    let end = prefix.end;
                    if end > 0 {
                        let text = self.remaining()[..end].to_owned();
                        self.consume(end);
                        self.line_has_text = false;
                        fragments.push(Fragment::Text {
                            text,
                            visible: true,
                        });
                    }
                }
                self.line_prefix_done = true;
            }
            let first = self.remaining().chars().next().unwrap_or('\n');
            if self.remaining().is_empty() {
                break;
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
                    let text = self.remaining()[..end].to_owned();
                    self.consume(end);
                    fragments.push(Fragment::Text {
                        text,
                        visible: true,
                    });
                    continue;
                }
                self.fence_close_candidate = false;
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
                            let text = self.remaining().to_owned();
                            self.consume(len);
                            fragments.push(Fragment::Text {
                                text,
                                visible: true,
                            });
                        } else {
                            self.consume(len);
                            fragments.push(Fragment::Incomplete);
                        }
                        continue;
                    }
                    tag::Scan::Tag { end, kind, closing }
                        if self.open.kind() != Some(Kind::Parameter)
                            || (closing && kind == Kind::Parameter) =>
                    {
                        let raw = self.remaining()[..end].to_owned();
                        let span = self.offset..self.offset + end;
                        let escaped = self.escaped;
                        let literal_block = self.literal_block;
                        let mut literal = if self.open.is_empty() {
                            escaped
                        } else {
                            self.open.visible()
                        };
                        if closing {
                            if !escaped {
                                literal |= self.open.close(kind).unwrap_or(false);
                            } else if !self.open.visible() {
                                self.consume(end);
                                fragments.push(Fragment::Text {
                                    text: raw,
                                    visible: false,
                                });
                                continue;
                            }
                        } else if !raw.trim_end_matches('>').trim_end().ends_with('/') {
                            self.open.push(kind, literal);
                        }
                        self.consume(end);
                        fragments.push(Fragment::Tag(Tag {
                            kind,
                            closing,
                            span,
                            raw,
                            literal,
                            literal_block,
                        }));
                        continue;
                    }
                    tag::Scan::Ordinary | tag::Scan::Tag { .. } => {}
                }
            }
            if self.open.is_empty()
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
                match self.literal {
                    Some((delimiter, length, fence))
                        if first == delimiter
                            && ((!fence && count == length) || (fence && fence_end)) =>
                    {
                        if fence_end {
                            self.fence_close_candidate = true;
                        } else {
                            self.literal = None;
                        }
                    }
                    None if first == '`' || (count >= 3 && !self.line_has_text) => {
                        let fence = count >= 3 && !self.line_has_text;
                        self.literal = Some((first, count, fence));
                        if fence {
                            self.fence_containers = self.line_containers.clone();
                            self.paragraph_open = false;
                        }
                    }
                    _ => {}
                }
                let text = self.remaining()[..count].to_owned();
                self.consume(count);
                fragments.push(Fragment::Text {
                    text,
                    visible: true,
                });
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
            let text = self.remaining()[..end].to_owned();
            self.consume(end);
            fragments.push(Fragment::Text {
                text,
                visible: self.open.visible(),
            });
        }
        self.pending.drain(..self.cursor);
        self.cursor = 0;
        fragments
    }

    fn consume(&mut self, end: usize) {
        for ch in self.pending[self.cursor..self.cursor + end].chars() {
            if ch == '\n' {
                let blank = !self.line_has_text;
                if !self.line_paragraph {
                    if matches!(self.literal, Some((_, _, false))) {
                        self.literal = None;
                    }
                    if !self.open.is_empty() && self.open.visible() {
                        self.open.clear();
                        self.literal_block += 1;
                    }
                }
                self.line_has_text = false;
                self.line_prefix_done = false;
                self.indented = false;
                self.prefix_cursor = markdown::Cursor::default();
                self.continuation_checked = 0;
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
#[derive(Debug, Default)]
pub struct DsmlToolCallStreamFilter {
    lexer: Lexer,
}

impl DsmlToolCallStreamFilter {
    pub fn push(&mut self, chunk: &str) -> String {
        self.project(chunk, false)
    }
    pub fn finish(&mut self) -> String {
        self.project("", true)
    }

    fn project(&mut self, chunk: &str, finish: bool) -> String {
        let mut visible = String::new();
        for fragment in self.lexer.push(chunk, finish) {
            match fragment {
                Fragment::Text {
                    text,
                    visible: true,
                } => visible.push_str(&text),
                Fragment::Tag(tag) if tag.literal => visible.push_str(&tag.raw),
                _ => {}
            }
        }
        visible
    }
}

/// Explicit DSML calls carry protocol intent without the ambiguous bare-XML
/// heuristic. An incomplete named invocation remains a failed input, not `{}`.
#[derive(Default)]
pub(super) struct Decoded {
    pub calls: Vec<(Range<usize>, Value)>,
    /// Invalid unnamed invocations still own all nested syntax families.
    pub owned: Vec<Range<usize>>,
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
        if opener.kind != Kind::Invoke || opener.closing {
            index += 1;
            continue;
        }
        let self_closed = opener.raw.trim_end_matches('>').trim_end().ends_with('/');
        // Determine ownership before validating the name. Every opener owns its
        // subtree, including unnamed/empty invocations and nested wrappers.
        let mut open = Scopes::default();
        open.push(Kind::Invoke, opener.literal);
        let boundary = if self_closed {
            None
        } else {
            tags[index + 1..]
                .iter()
                .position(|tag| {
                    if opener.literal && tag.literal_block != opener.literal_block {
                        return true;
                    }
                    if tag.closing {
                        if open.close(tag.kind).is_some() {
                            return open.is_empty();
                        }
                        // A wrapper not opened inside this invocation is its
                        // containing envelope; it terminates an incomplete call.
                        return tag.kind == Kind::Calls;
                    }
                    if !tag.raw.trim_end_matches('>').trim_end().ends_with('/') {
                        open.push(tag.kind, tag.literal);
                    }
                    false
                })
                .map(|offset| index + 1 + offset)
        };
        let closer = boundary.map(|index| &tags[index]).filter(|tag| {
            tag.kind == Kind::Invoke
                && tag.closing
                && (!opener.literal || tag.literal_block == opener.literal_block)
        });
        let complete = self_closed || closer.is_some();
        let body_end = if self_closed {
            opener.span.end
        } else {
            boundary
                .map(|index| tags[index].span.start)
                .unwrap_or(text.len())
        };
        let span_end = closer.map(|tag| tag.span.end).unwrap_or(body_end);
        decoded.owned.push(opener.span.start..span_end);
        if !opener.literal
            && let Some(name) =
                super::extract_attr(&opener.raw, "name").filter(|name| !name.is_empty())
        {
            let body = &text[opener.span.end..body_end];
            let parameters = tags[index + 1..]
                .iter()
                .take_while(|tag| tag.span.start < body_end)
                .collect::<Vec<_>>();
            let arguments = if complete {
                arguments(body, opener.span.end, &parameters)
            } else {
                None
            };
            // Retain the actual undecodable input so strict tool admission produces
            // its existing ToolInvalidArgs result before any executor is called.
            let arguments = arguments
                .map(|args| args.to_string())
                .unwrap_or_else(|| text[opener.span.start..span_end].to_owned());
            decoded.calls.push((
                opener.span.start..span_end,
                json!({
                    "id": format!("xmlfb_{}", Uuid::new_v4()), "type":"function",
                    "function":{"name":name,"arguments":arguments},
                }),
            ));
        }
        index = if self_closed {
            index + 1
        } else {
            boundary
                .map(|index| if closer.is_some() { index + 1 } else { index })
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
        let name = super::extract_attr(&tag.raw, "name")?;
        if args.contains_key(&name) {
            return None;
        }
        if !body.get(cursor..tag.span.start - base)?.trim().is_empty() {
            return None;
        }
        let value = body.get(tag.span.end - base..close.span.start - base)?;
        let value = match super::extract_attr(&tag.raw, "string").as_deref() {
            Some("false") => serde_json::from_str(value).ok()?,
            Some("true") | None => Value::String(value.to_owned()),
            _ => return None,
        };
        args.insert(name, value);
        cursor = close.span.end - base;
        index += 2;
    }
    body.get(cursor..)?
        .trim()
        .is_empty()
        .then_some(Value::Object(args))
}
