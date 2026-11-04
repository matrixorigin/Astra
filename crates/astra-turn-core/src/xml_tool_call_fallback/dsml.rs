//! DSML token boundaries shared by call recovery and visible-text projection.
//! Markdown literals are data; only explicit protocol tags outside them carry
//! tool intent. Raw provider bytes remain available to the existing admission
//! path, including named calls whose arguments cannot be decoded.

use regex::Regex;
use serde_json::{Value, json};
use std::{ops::Range, sync::OnceLock};
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
    literal: Option<(char, usize, bool)>,
    line_has_text: bool,
    escaped: bool,
    open: Vec<Kind>,
}

fn tag_regex() -> &'static Regex {
    static TAG: OnceLock<Regex> = OnceLock::new();
    TAG.get_or_init(|| Regex::new(
        r"(?is)^<\s*(/?)\s*(?:｜DSML｜|｜｜DSML｜｜|\|DSML\||\|\|DSML\|\|)\s*(tool_calls|function_calls|calls|invoke|parameter)\b[^<>]*>"
    ).expect("valid DSML token grammar"))
}

/// A prefix is held only while it can still become a supported protocol tag.
/// This also bounds normal-text latency when a chunk ends with `<`.
fn tag_prefix(text: &str) -> bool {
    let Some(tail) = text.strip_prefix('<') else {
        return false;
    };
    let tail = tail.trim_start();
    let tail = tail.strip_prefix('/').unwrap_or(tail).trim_start();
    for marker in ["｜DSML｜", "｜｜DSML｜｜", "|DSML|", "||DSML||"] {
        let mut chars = tail.char_indices();
        let mut matched = true;
        let mut end = 0;
        for expected in marker.chars() {
            match chars.next() {
                None => return true,
                Some((index, actual)) if actual.eq_ignore_ascii_case(&expected) => {
                    end = index + actual.len_utf8();
                }
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        let name = tail[end..].trim_start();
        return [
            "tool_calls",
            "function_calls",
            "calls",
            "invoke",
            "parameter",
        ]
        .iter()
        .any(|tag| {
            let length = name.len().min(tag.len());
            name.get(..length)
                .is_some_and(|prefix| tag[..length].eq_ignore_ascii_case(prefix))
                && (name.len() <= tag.len()
                    || matches!(
                        name.as_bytes()[tag.len()],
                        b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>'
                    ))
        });
    }
    false
}

#[derive(Clone, Debug)]
enum Container {
    Quote,
    List(usize),
}

struct Prefix {
    end: usize,
    indented: bool,
    containers: Vec<Container>,
}

fn indentation(bytes: &[u8], cursor: &mut usize) -> usize {
    let mut columns = 0;
    while let Some(ch) = bytes.get(*cursor) {
        match ch {
            b' ' => columns += 1,
            b'\t' => columns += 4 - columns % 4,
            _ => break,
        }
        *cursor += 1;
    }
    columns
}

/// Consume only Markdown container/indentation prefixes. Holding an unfinished
/// prefix lets streaming and complete-input decoding make the same decision.
/// Indented source is conservatively literal even when a paragraph could make
/// CommonMark interpret it as continuation text: it must never become a call.
fn markdown_prefix(text: &str, finish: bool) -> Option<Prefix> {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    let mut containers = Vec::new();
    loop {
        let indent = indentation(bytes, &mut cursor);
        if cursor == bytes.len() && !finish {
            return None;
        }
        if indent >= 4 {
            return Some(Prefix {
                end: cursor,
                indented: true,
                containers,
            });
        }
        if bytes.get(cursor) == Some(&b'>') {
            cursor += 1;
            if bytes.get(cursor) == Some(&b' ') {
                cursor += 1;
            }
            containers.push(Container::Quote);
            continue;
        }
        let marker_end = match bytes.get(cursor) {
            Some(b'-' | b'+' | b'*') => Some(cursor + 1),
            Some(b'0'..=b'9') => {
                let end = bytes[cursor..]
                    .iter()
                    .take_while(|ch| ch.is_ascii_digit())
                    .count()
                    + cursor;
                if end == bytes.len() && end - cursor <= 9 && !finish {
                    return None;
                }
                (end - cursor <= 9 && matches!(bytes.get(end), Some(b'.' | b')')))
                    .then_some(end + 1)
            }
            _ => None,
        };
        if let Some(end) = marker_end {
            if end == bytes.len() && !finish {
                return None;
            }
            if matches!(bytes.get(end), Some(b' ' | b'\t')) {
                let mut content = end;
                let padding = indentation(bytes, &mut content);
                if content == bytes.len() && !finish {
                    return None;
                }
                // CommonMark list padding is one to four columns. With more
                // padding, one column belongs to the marker and the remainder
                // starts an indented code block.
                let (content, padding) = if padding <= 4 {
                    (content, padding)
                } else {
                    (end + 1, 1)
                };
                containers.push(Container::List(indent + end - cursor + padding));
                cursor = content;
                continue;
            }
        }
        return Some(Prefix {
            end: cursor,
            indented: false,
            containers,
        });
    }
}

/// A fenced block consumes only the containers that opened it. A new quote or
/// list-looking line inside code is data, and leaving a container ends its fence.
/// None waits for more prefix bytes; Some(None) leaves the containing block.
fn fence_prefix(text: &str, finish: bool, containers: &[Container]) -> Option<Option<Prefix>> {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    for container in containers {
        match container {
            Container::Quote => {
                if indentation(bytes, &mut cursor) > 3 {
                    return Some(None);
                }
                if cursor == bytes.len() && !finish {
                    return None;
                }
                if bytes.get(cursor) != Some(&b'>') {
                    return Some(None);
                }
                cursor += 1;
                if bytes.get(cursor) == Some(&b' ') {
                    cursor += 1;
                }
            }
            Container::List(width) => {
                let mut columns = 0;
                while columns < *width {
                    match bytes.get(cursor) {
                        Some(b' ') => columns += 1,
                        Some(b'\t') => columns += 4 - columns % 4,
                        None if !finish => return None,
                        _ => return Some(None),
                    }
                    cursor += 1;
                }
            }
        }
    }
    let indent = indentation(bytes, &mut cursor);
    if cursor == bytes.len() && !finish {
        return None;
    }
    Some(Some(Prefix {
        end: cursor,
        indented: indent >= 4,
        containers: containers.to_vec(),
    }))
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
                if self.open.is_empty() {
                    let prefix = if matches!(self.literal, Some((_, _, true))) {
                        let Some(prefix) =
                            fence_prefix(self.remaining(), finish, &self.fence_containers)
                        else {
                            break;
                        };
                        if prefix.is_none() {
                            self.literal = None;
                        }
                        prefix
                    } else {
                        None
                    };
                    let Some(prefix) = prefix.or_else(|| markdown_prefix(self.remaining(), finish))
                    else {
                        break;
                    };
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
            if self.literal.is_none() && !self.indented && first == '<' {
                if let Some(captures) = tag_regex().captures(self.remaining()) {
                    let end = captures.get(0).unwrap().end();
                    let closing = &captures[1] == "/";
                    let kind = match captures[2].to_ascii_lowercase().as_str() {
                        "invoke" => Kind::Invoke,
                        "parameter" => Kind::Parameter,
                        _ => Kind::Calls,
                    };
                    // Tags inside a parameter value belong to the argument,
                    // never to another independently executable invocation.
                    if self.open.last() != Some(&Kind::Parameter)
                        || (closing && kind == Kind::Parameter)
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
                } else if tag_prefix(self.remaining()) && !self.remaining().contains('>') {
                    if !finish {
                        break;
                    }
                    let tail = self
                        .remaining()
                        .trim_start_matches('<')
                        .trim_start_matches('/')
                        .trim_start();
                    if tail.starts_with(['｜', '|']) {
                        let len = self.remaining().len();
                        self.consume(len);
                        fragments.push(Fragment::Incomplete);
                        continue;
                    }
                }
            }
            if self.open.is_empty()
                && !self.indented
                && matches!(first, '`' | '~')
                && (!self.escaped || self.literal.is_some())
            {
                let count = self
                    .remaining()
                    .chars()
                    .take_while(|ch| *ch == first)
                    .count();
                if count == self.remaining().len() && !finish {
                    break;
                }
                let fence_end = if let Some((delimiter, length, true)) = self.literal {
                    if first == delimiter && !self.line_has_text && count >= length {
                        let tail = &self.remaining()[count..];
                        let line = tail.split('\n').next().unwrap_or(tail);
                        if line.trim().is_empty() && !tail.contains('\n') && !finish {
                            break;
                        }
                        line.trim().is_empty()
                    } else {
                        false
                    }
                } else {
                    false
                };
                match self.literal {
                    Some((delimiter, length, fence))
                        if first == delimiter
                            && ((!fence && count == length) || (fence && fence_end)) =>
                    {
                        self.literal = None
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
