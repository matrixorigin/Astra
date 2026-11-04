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
    let compact = text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let Some(tail) = compact.strip_prefix('<') else {
        return false;
    };
    let tail = tail.strip_prefix('/').unwrap_or(tail);
    for marker in ["｜dsml｜", "｜｜dsml｜｜", "|dsml|", "||dsml||"] {
        if marker.starts_with(tail) {
            return true;
        }
        if let Some(name) = tail.strip_prefix(marker) {
            return [
                "tool_calls",
                "function_calls",
                "calls",
                "invoke",
                "parameter",
            ]
            .iter()
            .any(|tag| tag.starts_with(name) || name.starts_with(tag));
        }
    }
    false
}

impl Lexer {
    fn push(&mut self, chunk: &str, finish: bool) -> Vec<Fragment> {
        self.pending.push_str(chunk);
        let mut fragments = Vec::new();
        while !self.pending.is_empty() {
            let first = self.pending.chars().next().unwrap();
            if self.literal.is_none() && first == '<' {
                if let Some(captures) = tag_regex().captures(&self.pending) {
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
                        let raw = self.pending[..end].to_owned();
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
                } else if !self.pending.contains('>') && tag_prefix(&self.pending) {
                    if !finish {
                        break;
                    }
                    let tail = self
                        .pending
                        .trim_start_matches('<')
                        .trim_start_matches('/')
                        .trim_start();
                    if tail.starts_with(['｜', '|']) {
                        let len = self.pending.len();
                        self.consume(len);
                        fragments.push(Fragment::Incomplete);
                        continue;
                    }
                }
            }
            if self.open.is_empty()
                && matches!(first, '`' | '~')
                && (!self.escaped || self.literal.is_some())
            {
                let count = self.pending.chars().take_while(|ch| *ch == first).count();
                if count == self.pending.len() && !finish {
                    break;
                }
                let fence_end = if let Some((delimiter, length, true)) = self.literal {
                    if first == delimiter && !self.line_has_text && count >= length {
                        let tail = &self.pending[count..];
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
                        self.literal = Some((first, count, count >= 3 && !self.line_has_text));
                    }
                    _ => {}
                }
                let text = self.pending[..count].to_owned();
                self.consume(count);
                fragments.push(Fragment::Text {
                    text,
                    visible: true,
                });
                continue;
            }
            // Publish runs, not individual characters; a long ordinary answer
            // does not allocate one token or rescan its accumulated history.
            let end = self
                .pending
                .char_indices()
                .skip(1)
                .find(|(_, ch)| matches!(ch, '<' | '`' | '~' | '\n'))
                .map(|(index, _)| index)
                .unwrap_or(self.pending.len());
            let text = self.pending[..end].to_owned();
            self.consume(end);
            fragments.push(Fragment::Text {
                text,
                visible: self.open.is_empty(),
            });
        }
        fragments
    }

    fn consume(&mut self, end: usize) {
        for ch in self.pending[..end].chars() {
            if ch == '\n' {
                self.line_has_text = false;
            } else if !ch.is_whitespace() {
                self.line_has_text = true;
            }
            self.escaped = ch == '\\' && !self.escaped;
        }
        self.pending.drain(..end);
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
pub(super) fn calls(text: &str) -> Vec<(Range<usize>, Value)> {
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
    let mut calls = Vec::new();
    let mut index = 0;
    while index < tags.len() {
        let opener = &tags[index];
        if opener.kind != Kind::Invoke || opener.closing {
            index += 1;
            continue;
        }
        let Some(name) = super::extract_attr(&opener.raw, "name").filter(|name| !name.is_empty())
        else {
            index += 1;
            continue;
        };
        let self_closed = opener.raw.trim_end_matches('>').trim_end().ends_with('/');
        let mut nesting = 0;
        let boundary = if self_closed {
            None
        } else {
            tags[index + 1..]
                .iter()
                .position(|tag| {
                    if tag.kind == Kind::Calls && tag.closing {
                        return true;
                    }
                    if tag.kind != Kind::Invoke {
                        return false;
                    }
                    if tag.closing {
                        if nesting == 0 {
                            return true;
                        }
                        nesting -= 1;
                    } else if !tag.raw.trim_end_matches('>').trim_end().ends_with('/') {
                        nesting += 1;
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
        calls.push((
            opener.span.start..span_end,
            json!({
                "id": format!("xmlfb_{}", Uuid::new_v4()), "type":"function",
                "function":{"name":name,"arguments":arguments},
            }),
        ));
        index = if self_closed {
            index + 1
        } else {
            boundary
                .map(|index| if closer.is_some() { index + 1 } else { index })
                .unwrap_or(tags.len())
        };
    }
    calls
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
