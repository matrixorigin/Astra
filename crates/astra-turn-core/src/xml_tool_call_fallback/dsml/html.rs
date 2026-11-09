//! HTML literal ownership, identified by CommonMark rather than a code-tag
//! allowlist. Element matching is an ordered stack with indexed parent lookup.
use super::markdown::Container;
use std::{collections::HashMap, sync::Arc};

#[derive(Debug)]
struct Element {
    name: String,
    previous: Option<usize>,
}
#[derive(Debug)]
struct Block {
    end: BlockEnd,
    containers: Arc<[Container]>,
}
#[derive(Debug)]
pub(super) enum BlockEnd {
    Blank,
    ClosingTag(String),
    Line,
}

impl BlockEnd {
    pub fn for_tag(raw: &str, name: &str) -> Self {
        use pulldown_cmark::{Event, Parser};
        // The opener is already a CommonMark HTML block. Place the close on
        // a separate line so a standalone type-7 opener stays a block.
        let probe = format!("{raw}\n</{name}>\nplain");
        let marker = probe.len() - "plain".len();
        if Parser::new(&probe)
            .into_offset_iter()
            .any(|(event, range)| matches!(event, Event::Html(_)) && range.contains(&marker))
        {
            Self::Blank
        } else {
            Self::ClosingTag(format!("</{name}>"))
        }
    }
}
#[derive(Debug, Default)]
pub(super) struct Context {
    elements: Vec<Element>,
    last: HashMap<String, usize>,
    block: Option<Block>,
}
impl Context {
    pub fn active(&self) -> bool {
        !self.elements.is_empty() || self.block.is_some()
    }
    pub fn containers(&self) -> Option<&[Container]> {
        self.block.as_ref().map(|block| block.containers.as_ref())
    }
    pub fn clear(&mut self) {
        self.elements.clear();
        self.last.clear();
        self.block = None;
    }
    pub fn newline(&mut self, blank: bool) {
        if match self.block.as_ref().map(|block| &block.end) {
            None | Some(BlockEnd::Blank) => blank,
            Some(BlockEnd::Line) => true,
            Some(BlockEnd::ClosingTag(_)) => false,
        } {
            self.clear();
        }
    }
    pub fn start_block(&mut self, end: BlockEnd, containers: Arc<[Container]>) {
        self.block = Some(Block { end, containers });
    }
    /// Block contents are raw HTML. Only a block's own end rule can change
    /// ownership, even if a close spelling occurs inside an apparent tag.
    pub fn block_prefix(&mut self, text: &str, finish: bool) -> Option<usize> {
        let block = self.block.as_mut()?;
        let newline = text.find('\n');
        let line = newline.map_or(text, |index| &text[..=index]);
        if let BlockEnd::ClosingTag(close) = &block.end {
            if line
                .as_bytes()
                .windows(close.len())
                .any(|bytes| bytes.eq_ignore_ascii_case(close.as_bytes()))
            {
                block.end = BlockEnd::Line;
            } else if newline.is_none() && !finish {
                let mut safe = line.len().saturating_sub(close.len() - 1);
                while !line.is_char_boundary(safe) {
                    safe -= 1;
                }
                return Some(safe);
            }
        }
        Some(line.len())
    }
    pub fn tag(&mut self, raw: &str, name: &str, closing: bool) {
        if closing {
            if let Some(index) = self.last.get(name).copied() {
                while self.elements.len() > index {
                    let frame = self.elements.pop().unwrap();
                    if let Some(previous) = frame.previous {
                        self.last.insert(frame.name, previous);
                    } else {
                        self.last.remove(&frame.name);
                    }
                }
            }
            return;
        }
        // HTML's void-element grammar has no matching end tag.
        if raw.trim_end_matches('>').trim_end().ends_with('/')
            || matches!(
                name,
                "area"
                    | "base"
                    | "br"
                    | "col"
                    | "embed"
                    | "hr"
                    | "img"
                    | "input"
                    | "link"
                    | "meta"
                    | "param"
                    | "source"
                    | "track"
                    | "wbr"
            )
            || name.starts_with('!')
        {
            return;
        }
        let previous = self.last.insert(name.to_owned(), self.elements.len());
        self.elements.push(Element {
            name: name.to_owned(),
            previous,
        });
    }
}
