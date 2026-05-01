use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};

pub(crate) trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }
}

impl Renderable for Paragraph<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Widget::render(self.clone(), area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.line_count(width) as u16
    }
}

pub(crate) struct ColumnRenderable<'a> {
    pub children: Vec<Box<dyn Renderable + 'a>>,
}

#[allow(dead_code)]
impl<'a> ColumnRenderable<'a> {
    pub fn new(children: Vec<Box<dyn Renderable + 'a>>) -> Self {
        Self { children }
    }
}

impl Renderable for ColumnRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        for child in &self.children {
            let h = child.desired_height(area.width);
            let child_area = Rect::new(area.x, y, area.width, h.min(area.bottom().saturating_sub(y)));
            if child_area.height == 0 {
                break;
            }
            child.render(child_area, buf);
            y = y.saturating_add(h);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.children.iter().map(|c| c.desired_height(width)).sum()
    }
}

impl Renderable for () {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}
    fn desired_height(&self, _width: u16) -> u16 {
        0
    }
}

impl Renderable for &str {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(Text::raw(*self))
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::raw(*self))
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }
}

impl Renderable for Line<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.clone())
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(self.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16
    }
}

impl Renderable for Span<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Line::from(self.clone()).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        Line::from(self.clone()).desired_height(width)
    }
}
