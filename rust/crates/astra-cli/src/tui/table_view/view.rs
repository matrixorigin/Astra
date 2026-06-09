//! Table widget renderer — uses ratatui's `Table` widget with a
//! selection highlight and width-aware truncation.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Widget};

use super::parser::MysqlTable;
use super::TableNav;

pub(crate) const MAX_CELL_WIDTH: u16 = 24;

pub(crate) fn desired_height(table: &MysqlTable) -> u16 {
    // border(2) + header(1) + rows (bounded) + hint(1)
    if table.headers.is_empty() {
        return 3;
    }
    let rows = (table.rows.len() as u16).clamp(1, 14);
    rows + 4
}

pub(crate) fn render(table: &MysqlTable, nav: &TableNav, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = title_line(table, nav);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    let inner = outer.inner(area);
    outer.render(area, buf);

    if table.headers.is_empty() {
        Paragraph::new(Line::from(Span::styled(
            "  no table data to show",
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(inner, buf);
        return;
    }

    // The `/table` panel relies on BottomPane's unified hint bar
    // for keybinding help, so we don't reserve a hint row inside
    // the bordered box — use the full inner area for the table.
    let table_rect = inner;

    let visible_cols = visible_cols(table, nav, inner.width);

    let header_cells: Vec<Cell<'static>> = visible_cols
        .iter()
        .map(|&c| {
            Cell::from(truncate_label(&table.headers[c], MAX_CELL_WIDTH as usize)).style(
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    let header = Row::new(header_cells);

    let body: Vec<Row<'static>> = table
        .rows
        .iter()
        .map(|r| {
            let cells: Vec<Cell<'static>> = visible_cols
                .iter()
                .map(|&c| {
                    let v = r.get(c).cloned().unwrap_or_default();
                    Cell::from(truncate_label(&v, MAX_CELL_WIDTH as usize))
                })
                .collect();
            Row::new(cells)
        })
        .collect();

    let widths: Vec<Constraint> = visible_cols
        .iter()
        .map(|_| Constraint::Length(MAX_CELL_WIDTH))
        .collect();

    let mut state = TableState::default();
    state.select(Some(nav.row));

    let theme = crate::tui::theme::current();
    let table_widget = Table::new(body, widths).header(header).row_highlight_style(
        Style::default()
            .bg(theme.accent)
            .fg(theme.selected_fg)
            .add_modifier(Modifier::BOLD),
    );

    ratatui::widgets::StatefulWidget::render(table_widget, table_rect, buf, &mut state);
}

fn title_line(table: &MysqlTable, nav: &TableNav) -> Line<'static> {
    let row_label = if table.rows.is_empty() {
        "".to_string()
    } else {
        format!("row {}/{}", nav.row + 1, table.rows.len())
    };
    let cols = table.headers.len();
    let col_label = if cols > 0 && nav.col_offset > 0 {
        format!(" · cols {}–{}/{cols}", nav.col_offset + 1, cols)
    } else {
        String::new()
    };
    Line::from(Span::styled(
        format!(
            " Table · {} rows × {} cols{}{}",
            table.rows.len(),
            table.headers.len(),
            if row_label.is_empty() {
                String::new()
            } else {
                format!(" · {row_label}")
            },
            col_label,
        ),
        Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD),
    ))
}

/// Indexes of columns to display given the current col_offset and the
/// available width.
fn visible_cols(table: &MysqlTable, nav: &TableNav, width: u16) -> Vec<usize> {
    let col_w = MAX_CELL_WIDTH + 1; // include separator
    let fit = (width / col_w).max(1) as usize;
    let total = table.headers.len();
    if total == 0 {
        return Vec::new();
    }
    let start = nav.col_offset.min(total.saturating_sub(1));
    let end = (start + fit).min(total);
    (start..end).collect()
}

use crate::cli::effects::truncate_label;

#[cfg(test)]
mod tests {
    use super::super::parser::parse;
    use super::{desired_height, render, truncate_label};
    use crate::tui::table_view::{MysqlTable, TableNav};
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    const SAMPLE: &str = "\
+----+--------+--------------+
| id | name   | email        |
+----+--------+--------------+
|  1 | alice  | a@example.co |
|  2 | bob    | b@example.co |
|  3 | carol  | c@example.co |
+----+--------+--------------+
";

    fn fixture() -> MysqlTable {
        parse(SAMPLE).expect("parse sample")
    }

    struct W<'a>(&'a MysqlTable, &'a TableNav);
    impl Widget for W<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, self.1, area, buf);
        }
    }

    fn draw(t: &MysqlTable, n: &TableNav, w: u16, h: u16) -> String {
        let buf = draw_widget(W(t, n), w, h);
        buffer_to_string(&buf)
    }

    #[test]
    fn snapshot_basic_three_row_table() {
        let t = fixture();
        let n = TableNav::new(t.num_rows(), t.num_cols());
        crate::tui::testing::assert_tui_snapshot!("table_basic_100x8", draw(&t, &n, 100, 8));
    }

    #[test]
    fn snapshot_second_row_selected() {
        let t = fixture();
        let mut n = TableNav::new(t.num_rows(), t.num_cols());
        n.move_down();
        crate::tui::testing::assert_tui_snapshot!(
            "table_row2_selected_100x8",
            draw(&t, &n, 100, 8)
        );
    }

    #[test]
    fn snapshot_narrow_shows_subset_cols() {
        let t = fixture();
        let n = TableNav::new(t.num_rows(), t.num_cols());
        crate::tui::testing::assert_tui_snapshot!("table_narrow_50x8", draw(&t, &n, 50, 8));
    }

    #[test]
    fn snapshot_scrolled_right() {
        let t = fixture();
        let mut n = TableNav::new(t.num_rows(), t.num_cols());
        n.scroll_right();
        crate::tui::testing::assert_tui_snapshot!("table_scrolled_right_60x8", draw(&t, &n, 60, 8));
    }

    #[test]
    fn snapshot_empty_table() {
        let t = MysqlTable {
            headers: Vec::new(),
            rows: Vec::new(),
        };
        let n = TableNav::new(0, 0);
        crate::tui::testing::assert_tui_snapshot!("table_empty_80x3", draw(&t, &n, 80, 3));
    }

    #[test]
    fn snapshot_truncates_wide_values() {
        let wide = "\
+----+-----------------------------+
| id | description                 |
+----+-----------------------------+
|  1 | this is a very long string  |
|  2 | and another equally verbose |
+----+-----------------------------+
";
        let t = parse(wide).unwrap();
        let n = TableNav::new(t.num_rows(), t.num_cols());
        crate::tui::testing::assert_tui_snapshot!("table_truncates_80x8", draw(&t, &n, 80, 8));
    }

    // Helpers

    #[test]
    fn truncate_produces_ellipsis() {
        assert_eq!(truncate_label("hello", 10), "hello");
        assert_eq!(truncate_label("hello world", 5), "hell…");
        assert_eq!(truncate_label("", 3), "");
    }

    #[test]
    fn desired_height_scales_with_rows() {
        let t = fixture();
        // 3 rows → 3 + 4 = 7.
        assert_eq!(desired_height(&t), 7);
    }
}
