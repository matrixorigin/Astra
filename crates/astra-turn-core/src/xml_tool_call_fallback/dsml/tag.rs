//! One resumable lexical grammar for tool markup and surrounding HTML.
//! Each pending tag byte is consumed once, including quoted attributes.
use super::{Family, Kind};

const MARKERS: [&str; 4] = ["｜DSML｜", "｜｜DSML｜｜", "|DSML|", "||DSML||"];

#[derive(Debug, Default)]
enum Stage {
    #[default]
    Start,
    Marker {
        candidates: u8,
        position: usize,
    },
    NameStart,
    Name,
    Attributes,
}

#[derive(Debug, Default)]
pub(super) struct Scanner {
    checked: usize,
    stage: Stage,
    closing: bool,
    family: Family,
    name: String,
    quote: Option<char>,
    attributes_start: Option<usize>,
}

/// Only attributes lexically present on this opening tag can supply call facts.
/// Values inside another attribute or descendant content are never searched.
#[derive(Debug, Default)]
pub(super) struct Attributes {
    pub name: Option<String>,
    pub string: Option<String>,
}

impl Attributes {
    fn parse(mut text: &str) -> Option<Self> {
        text = text.trim().strip_suffix('/').unwrap_or(text.trim()).trim();
        let mut attributes = Self::default();
        while !text.is_empty() {
            let end = text
                .bytes()
                .take_while(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b':' | b'.')
                })
                .count();
            if end == 0 {
                return None;
            }
            let key = &text[..end];
            text = text[end..].trim_start().strip_prefix('=')?.trim_start();
            let quote = text.chars().next()?;
            if !matches!(quote, '\'' | '"') {
                return None;
            }
            text = &text[1..];
            let end = text.find(quote)?;
            let value = &text[..end];
            let field = match key {
                "name" => Some(&mut attributes.name),
                "string" => Some(&mut attributes.string),
                _ => None,
            };
            if let Some(field) = field {
                if field.is_some() {
                    return None;
                }
                *field = Some(value.to_owned());
            }
            text = &text[end + 1..];
            if text.chars().next().is_some_and(|ch| !ch.is_whitespace()) {
                return None;
            }
            text = text.trim_start();
        }
        Some(attributes)
    }
}

pub(super) enum Scan {
    Pending,
    Ordinary,
    Incomplete,
    Opaque {
        end: usize,
        terminator: &'static str,
    },
    Html {
        end: usize,
        name: String,
        closing: bool,
    },
    Tag {
        end: usize,
        kind: Kind,
        family: Family,
        closing: bool,
        attributes: Option<Attributes>,
    },
}

impl Scanner {
    fn complete(&self, end: usize, text: &str) -> Scan {
        let kind = match self.name.as_str() {
            "invoke" => Some(Kind::Invoke),
            "parameter" => Some(Kind::Parameter),
            "tool_call" if self.family == Family::Bare => Some(Kind::ToolCall),
            "tool_calls" | "function_calls" | "calls" if self.family == Family::Dsml => {
                Some(Kind::Calls)
            }
            _ => None,
        };
        if let Some(kind) = kind {
            Scan::Tag {
                end,
                kind,
                family: self.family,
                closing: self.closing,
                attributes: self.attributes_start.map_or_else(
                    || Some(Attributes::default()),
                    |start| Attributes::parse(&text[start..end - 1]),
                ),
            }
        } else if self.family == Family::Dsml {
            Scan::Ordinary
        } else {
            use pulldown_cmark::{Event, Parser};
            if Parser::new(&text[..end])
                .any(|event| matches!(event, Event::Html(_) | Event::InlineHtml(_)))
            {
                Scan::Html {
                    end,
                    name: self.name.clone(),
                    closing: self.closing,
                }
            } else {
                Scan::Ordinary
            }
        }
    }

    pub(super) fn scan(&mut self, text: &str, finish: bool) -> Scan {
        if self.checked == 0 {
            for (opener, terminator) in [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>")] {
                if text.starts_with(opener) {
                    return Scan::Opaque {
                        end: opener.len(),
                        terminator,
                    };
                }
                if opener.starts_with(text) && !finish {
                    return Scan::Pending;
                }
            }
            self.checked = 1;
        }
        for (index, ch) in text[self.checked..].char_indices() {
            let end = self.checked + index + ch.len_utf8();
            match &mut self.stage {
                Stage::Start if ch.is_whitespace() => {}
                Stage::Start if ch == '/' && !self.closing => {
                    self.closing = true;
                }
                Stage::Start => {
                    let candidates = MARKERS.iter().enumerate().fold(0, |bits, (i, marker)| {
                        bits | if marker.starts_with(ch) { 1 << i } else { 0 }
                    });
                    if candidates != 0 {
                        self.family = Family::Dsml;
                        self.stage = Stage::Marker {
                            candidates,
                            position: 1,
                        };
                    } else if ch.is_ascii_alphabetic() || ch == '!' {
                        self.name.push(ch.to_ascii_lowercase());
                        self.stage = Stage::Name;
                    } else {
                        return Scan::Ordinary;
                    }
                }
                Stage::Marker {
                    candidates,
                    position,
                } => {
                    let mut remaining = 0;
                    let mut complete = false;
                    for (i, marker) in MARKERS.iter().enumerate() {
                        if *candidates & (1 << i) != 0
                            && marker
                                .chars()
                                .nth(*position)
                                .is_some_and(|expected| expected.eq_ignore_ascii_case(&ch))
                        {
                            remaining |= 1 << i;
                            complete |= marker.chars().count() == *position + 1;
                        }
                    }
                    if remaining == 0 {
                        return Scan::Ordinary;
                    }
                    if complete {
                        self.stage = Stage::NameStart;
                    } else {
                        *candidates = remaining;
                        *position += 1;
                    }
                }
                Stage::NameStart if ch.is_whitespace() => {}
                Stage::NameStart => {
                    if !ch.is_ascii_alphabetic() {
                        return Scan::Ordinary;
                    }
                    self.name.push(ch.to_ascii_lowercase());
                    self.stage = Stage::Name;
                }
                Stage::Name => {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':' | '.') {
                        self.name.push(ch.to_ascii_lowercase());
                    } else if ch == '>' {
                        return self.complete(end, text);
                    } else if ch.is_whitespace() || ch == '/' {
                        self.attributes_start = Some(end - ch.len_utf8());
                        self.stage = Stage::Attributes;
                    } else {
                        return Scan::Ordinary;
                    }
                }
                Stage::Attributes => {
                    if let Some(quote) = self.quote {
                        if ch == quote {
                            self.quote = None;
                        }
                    } else if matches!(ch, '\'' | '"') {
                        self.quote = Some(ch);
                    } else if ch == '>' {
                        return self.complete(end, text);
                    } else if ch == '<' {
                        return Scan::Ordinary;
                    }
                }
            }
        }
        self.checked = text.len();
        if finish {
            if self.family == Family::Dsml {
                Scan::Incomplete
            } else {
                Scan::Ordinary
            }
        } else {
            Scan::Pending
        }
    }
}
