use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Widget,
};

const MAX_VISIBLE: usize = 10;

#[derive(Clone)]
pub(crate) struct SkillItem {
    pub name: String,
    pub description: String,
    pub source: String,
}

pub(crate) struct SkillPopup {
    items: Vec<SkillItem>,
    filter: String,
    selected: usize,
}

impl SkillPopup {
    pub fn new(items: Vec<SkillItem>) -> Self {
        Self {
            items,
            filter: String::new(),
            selected: 0,
        }
    }

    pub fn set_filter(&mut self, text: &str) {
        let first_line = text.lines().next().unwrap_or("");
        self.filter = first_line
            .strip_prefix('$')
            .unwrap_or("")
            .to_lowercase();
        if self.selected >= self.filtered().len().max(1) {
            self.selected = 0;
        }
    }

    fn filtered(&self) -> Vec<&SkillItem> {
        if self.filter.is_empty() {
            self.items.iter().collect()
        } else {
            self.items
                .iter()
                .filter(|item| {
                    item.name.to_lowercase().contains(&self.filter)
                        || item.description.to_lowercase().contains(&self.filter)
                })
                .collect()
        }
    }

    pub fn move_up(&mut self) {
        let len = self.filtered().len();
        if len > 0 {
            self.selected = if self.selected == 0 { len - 1 } else { self.selected - 1 };
        }
    }

    pub fn move_down(&mut self) {
        let len = self.filtered().len();
        if len > 0 {
            self.selected = (self.selected + 1) % len;
        }
    }

    pub fn selected_name(&self) -> Option<String> {
        self.filtered().get(self.selected).map(|i| i.name.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.filtered().is_empty()
    }

    pub fn height(&self) -> u16 {
        let n = self.filtered().len();
        if n == 0 { return 0; }
        let items_h = n.min(MAX_VISIBLE) as u16;
        items_h + 2 // items + blank + hint
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let filtered = self.filtered();
        if filtered.is_empty() {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);

        let visible_start = if self.selected >= MAX_VISIBLE {
            self.selected - MAX_VISIBLE + 1
        } else {
            0
        };
        let visible_end = (visible_start + MAX_VISIBLE).min(filtered.len());

        let mut y = area.y;
        for (_vi, i) in (visible_start..visible_end).enumerate() {
            if y >= area.bottom() { break; }
            let item = filtered[i];

            let tag = format!("[{}]", item.source);
            let name_w = 18;
            let tag_w = tag.len() + 1;
            let padded_name = format!("{:<width$}", item.name, width = name_w);
            let desc_budget = (area.width as usize).saturating_sub(2 + name_w + tag_w + 1);
            let desc: String = item.description.chars().take(desc_budget).collect();

            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(padded_name, dim),
                Span::styled(format!("{tag} "), Style::default().fg(Color::Cyan)),
                Span::styled(desc, dim),
            ]);
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank + hint
        if y < area.bottom() { y += 1; }
        if y < area.bottom() {
            let hint = Line::from(Span::styled(
                "  Press enter to insert or esc to close",
                dim,
            ));
            Widget::render(hint, Rect::new(area.x, y, area.width, 1), buf);
        }
    }
}
