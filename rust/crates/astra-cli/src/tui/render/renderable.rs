// Ported from Codex CLI (MIT license). Implements FlexRenderable for Codex-style layout.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{Insets, RectExt as _};

pub(crate) trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }
}

pub(crate) enum RenderableItem<'a> {
    Owned(Box<dyn Renderable + 'a>),
    #[allow(dead_code)]
    Borrowed(&'a dyn Renderable),
}

impl<'a> Renderable for RenderableItem<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        match self {
            RenderableItem::Owned(c) => c.render(area, buf),
            RenderableItem::Borrowed(c) => c.render(area, buf),
        }
    }
    fn desired_height(&self, width: u16) -> u16 {
        match self {
            RenderableItem::Owned(c) => c.desired_height(width),
            RenderableItem::Borrowed(c) => c.desired_height(width),
        }
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        match self {
            RenderableItem::Owned(c) => c.cursor_pos(area),
            RenderableItem::Borrowed(c) => c.cursor_pos(area),
        }
    }
}

impl Renderable for () {
    fn render(&self, _: Rect, _: &mut Buffer) {}
    fn desired_height(&self, _: u16) -> u16 {
        0
    }
}

impl Renderable for Paragraph<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        ratatui::widgets::Widget::render(self.clone(), area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.line_count(width) as u16
    }
}

impl Renderable for Line<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        ratatui::widgets::WidgetRef::render_ref(self, area, buf);
    }
    fn desired_height(&self, _: u16) -> u16 {
        1
    }
}

impl Renderable for Span<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Line::from(self.clone()).render(area, buf);
    }
    fn desired_height(&self, _: u16) -> u16 {
        1
    }
}

// ── FlexRenderable (from Codex) ─────────────────────────────────────────────

struct FlexChild<'a> {
    flex: i32,
    child: RenderableItem<'a>,
}

pub(crate) struct FlexRenderable<'a> {
    children: Vec<FlexChild<'a>>,
}

impl<'a> FlexRenderable<'a> {
    pub fn new() -> Self {
        Self { children: vec![] }
    }

    pub fn push(&mut self, flex: i32, child: RenderableItem<'a>) {
        self.children.push(FlexChild { flex, child });
    }

    fn allocate(&self, area: Rect) -> Vec<Rect> {
        let mut child_sizes = vec![0u16; self.children.len()];
        let mut allocated_size: u16 = 0;
        let mut total_flex: i32 = 0;
        let max_size = area.height;
        let mut last_flex_idx = 0;

        // 1. Non-flex children get their desired_height
        for (i, FlexChild { flex, child }) in self.children.iter().enumerate() {
            if *flex > 0 {
                total_flex += flex;
                last_flex_idx = i;
            } else {
                child_sizes[i] = child
                    .desired_height(area.width)
                    .min(max_size.saturating_sub(allocated_size));
                allocated_size += child_sizes[i];
            }
        }

        // 2. Flex children split remaining space
        let free_space = max_size.saturating_sub(allocated_size);
        let mut allocated_flex: u16 = 0;
        if total_flex > 0 {
            let per_flex = free_space / total_flex as u16;
            for (i, FlexChild { flex, child }) in self.children.iter().enumerate() {
                if *flex > 0 {
                    let max_extent = if i == last_flex_idx {
                        free_space - allocated_flex
                    } else {
                        per_flex * *flex as u16
                    };
                    let size = child.desired_height(area.width).min(max_extent);
                    child_sizes[i] = size;
                    allocated_flex += size;
                }
            }
        }

        let mut y = area.y;
        let mut rects = Vec::with_capacity(self.children.len());
        for size in child_sizes {
            rects.push(Rect::new(area.x, y, area.width, size));
            y += size;
        }
        rects
    }
}

impl<'a> Renderable for FlexRenderable<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        for (rect, child) in self.allocate(area).into_iter().zip(self.children.iter()) {
            child.child.render(rect, buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.allocate(Rect::new(0, 0, width, u16::MAX))
            .last()
            .map(|r| r.bottom())
            .unwrap_or(0)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.allocate(area)
            .into_iter()
            .zip(self.children.iter())
            .find_map(|(rect, child)| child.child.cursor_pos(rect))
    }
}

// ── InsetRenderable (from Codex) ────────────────────────────────────────────

pub(crate) struct InsetRenderable<'a> {
    child: RenderableItem<'a>,
    insets: Insets,
}

impl<'a> Renderable for InsetRenderable<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.child.render(area.inset(self.insets), buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.child
            .desired_height(width.saturating_sub(self.insets.left + self.insets.right))
            + self.insets.top
            + self.insets.bottom
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.child.cursor_pos(area.inset(self.insets))
    }
}

pub(crate) trait RenderableExt<'a> {
    fn inset(self, insets: Insets) -> RenderableItem<'a>;
}

impl<'a> RenderableExt<'a> for RenderableItem<'a> {
    fn inset(self, insets: Insets) -> RenderableItem<'a> {
        RenderableItem::Owned(Box::new(InsetRenderable {
            child: self,
            insets,
        }))
    }
}
