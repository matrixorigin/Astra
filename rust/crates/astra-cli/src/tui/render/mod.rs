pub(crate) mod highlight;
pub(crate) mod line_utils;
pub(crate) mod renderable;

use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Insets {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

#[allow(dead_code)]
impl Insets {
    pub const fn tlbr(top: u16, left: u16, bottom: u16, right: u16) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn horizontal(left: u16, right: u16) -> Self {
        Self {
            left,
            top: 0,
            right,
            bottom: 0,
        }
    }
}

pub(crate) trait RectExt {
    fn inset(&self, insets: Insets) -> Rect;
}

impl RectExt for Rect {
    fn inset(&self, insets: Insets) -> Rect {
        let x = self.x.saturating_add(insets.left);
        let y = self.y.saturating_add(insets.top);
        let w = self
            .width
            .saturating_sub(insets.left)
            .saturating_sub(insets.right);
        let h = self
            .height
            .saturating_sub(insets.top)
            .saturating_sub(insets.bottom);
        Rect::new(x, y, w, h)
    }
}
