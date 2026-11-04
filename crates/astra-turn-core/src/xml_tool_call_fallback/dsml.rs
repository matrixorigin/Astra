//! DSML token boundaries shared by call recovery and visible-text projection.
//! Markdown literals are data; only explicit protocol tags outside them carry
//! tool intent. Raw provider bytes remain available to the existing admission
//! path, including named calls whose arguments cannot be decoded.

use serde_json::{Value, json};
use std::ops::Range;

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
    line_containers: Vec<Container>,
    fence_containers: Vec<Container>,
    prefix_cursor: markdown::Cursor,
    tag_scanner: Option<tag::Scanner>,
    delimiter_run: usize,
    fence_close_candidate: bool,
    literal: Option<(char, usize, bool)>,
    line_has_text: bool,
    escaped: bool,
    open: Vec<Kind>,
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
                if self.open.is_empty() && !matches!(self.literal, Some((_, _, false))) {
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
                    let Some(prefix) = prefix.or_else(|| {
                        self.prefix_cursor
                            .normal(&self.pending[self.cursor..], finish)
                    }) else {
                        break;
                    };
                    self.prefix_cursor = markdown::Cursor::default();
                    self.indented = prefix.indented;
                    self.line_containers = prefix.containers;
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
                        self.consume(len);
                        fragments.push(Fragment::Incomplete);
                        continue;
                    }
                    tag::Scan::Tag { end, kind, closing }
                        if self.open.last() != Some(&Kind::Parameter)
                            || (closing && kind == Kind::Parameter) =>
                    {
                        let raw = self.remaining()[..end].to_owned();
                        let span = self.offset..self.offset + end;
                        if closing {
                            if let Some(index) = self.open.iter().rposition(|open| *open == kind) {
                                self.open.truncate(index);
                            }
                        } else if !raw.trim_end_matches('>').trim_end().ends_with('/') {
                            self.open.push(kind);
                        }
                        self.consume(end);
                        fragments.push(Fragment::Tag(Tag {
                            kind,
                            closing,
                            span,
                            raw,
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
                visible: self.open.is_empty(),
            });
        }
        self.pending.drain(..self.cursor);
        self.cursor = 0;
        fragments
    }

    fn consume(&mut self, end: usize) {
        for ch in self.pending[self.cursor..self.cursor + end].chars() {
            if ch == '\n' {
                self.line_has_text = false;
                self.line_prefix_done = false;
                self.indented = false;
                self.prefix_cursor = markdown::Cursor::default();
            } else if !ch.is_whitespace() {
                self.line_has_text = true;
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
            if let Fragment::Text {
                text,
                visible: true,
            } = fragment
            {
                visible.push_str(&text);
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
        let mut open = vec![Kind::Invoke];
        let boundary = if self_closed {
            None
        } else {
            tags[index + 1..]
                .iter()
                .position(|tag| {
                    if tag.closing {
                        if let Some(parent) = open.iter().rposition(|kind| *kind == tag.kind) {
                            open.truncate(parent);
                            return open.is_empty();
                        }
                        // A wrapper not opened inside this invocation is its
                        // containing envelope; it terminates an incomplete call.
                        return tag.kind == Kind::Calls;
                    }
                    if !tag.raw.trim_end_matches('>').trim_end().ends_with('/') {
                        open.push(tag.kind);
                    }
                    false
                })
                .map(|offset| index + 1 + offset)
        };
        let closer = boundary
            .map(|index| &tags[index])
            .filter(|tag| tag.kind == Kind::Invoke && tag.closing);
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
        if let Some(name) = super::extract_attr(&opener.raw, "name").filter(|name| !name.is_empty())
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
