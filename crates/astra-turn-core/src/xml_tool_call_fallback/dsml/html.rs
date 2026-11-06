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
    until_blank: bool,
    containers: Arc<[Container]>,
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
        if blank && self.block.as_ref().is_none_or(|block| block.until_blank) {
            self.clear();
        }
    }
    pub fn tag(
        &mut self,
        raw: &str,
        name: &str,
        closing: bool,
        line_start: bool,
        containers: Arc<[Container]>,
    ) {
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
                if self.elements.is_empty()
                    && self.block.as_ref().is_some_and(|block| !block.until_blank)
                {
                    self.block = None;
                }
            }
            return;
        }
        if line_start && self.block.is_none() {
            use pulldown_cmark::{Event, Parser, Tag};
            if matches!(Parser::new(raw).next(), Some(Event::Start(Tag::HtmlBlock))) {
                // Ask CommonMark whether this block ends on its closing element
                // or only at a blank line. This includes arbitrary HTML elements.
                let probe = format!("{raw}</{name}>\nplain");
                let marker = probe.len() - "plain".len();
                let until_blank = Parser::new(&probe)
                    .into_offset_iter()
                    .any(|(event, range)| {
                        matches!(event, Event::Html(_)) && range.contains(&marker)
                    });
                self.block = Some(Block {
                    until_blank,
                    containers,
                });
            }
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
