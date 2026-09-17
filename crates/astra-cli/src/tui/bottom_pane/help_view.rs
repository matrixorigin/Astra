use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::cli::command_registry::{self, CommandGroup, CommandMeta};

const MAX_CMD_ROWS: usize = 10;
const TAB_ROWS: u16 = 1;
const COMMAND_SPACER_ROWS: u16 = 1;
const HINT_SPACER_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
const MIN_VIEW_HEIGHT: u16 = TAB_ROWS + COMMAND_SPACER_ROWS + 1 + HINT_ROWS;

fn hint_spacer_rows_for_height(height: u16) -> u16 {
    if height > MIN_VIEW_HEIGHT {
        HINT_SPACER_ROWS
    } else {
        0
    }
}

fn visible_command_rows(height: u16, command_count: usize) -> usize {
    let hint_rows = if height >= MIN_VIEW_HEIGHT {
        HINT_ROWS
    } else {
        0
    };
    let reserved_rows =
        TAB_ROWS + COMMAND_SPACER_ROWS + hint_spacer_rows_for_height(height) + hint_rows;
    (height.saturating_sub(reserved_rows) as usize)
        .min(MAX_CMD_ROWS)
        .min(command_count)
}

fn help_hint() -> String {
    format!(
        "←/→ switch section  ↑/↓ browse  Enter select  /<name> searches all actions  {} background  Esc close",
        crate::tui::background_shortcut::ctrl_b_background_shortcut()
    )
}

struct GroupData {
    title: &'static str,
    commands: Vec<&'static CommandMeta>,
}

pub(crate) struct HelpView {
    groups: Vec<GroupData>,
    active_tab: usize,
    selected_cmd: usize,
    completed: bool,
    accepted: Option<String>,
}

impl HelpView {
    pub fn new() -> Self {
        let mut groups = Vec::with_capacity(CommandGroup::ALL.len() + 1);
        let featured: Vec<&'static CommandMeta> = command_registry::tui_commands()
            .filter(|m| m.is_primary() && !m.name.contains(' '))
            .collect();
        if !featured.is_empty() {
            groups.push(GroupData {
                title: "Featured",
                commands: featured,
            });
        }
        groups.extend(CommandGroup::ALL.iter().filter_map(|&group| {
            let commands: Vec<&'static CommandMeta> = command_registry::commands_by_group(group)
                .filter(|m| !m.name.contains(' '))
                .collect();
            (!commands.is_empty()).then_some(GroupData {
                title: group.title(),
                commands,
            })
        }));
        Self {
            groups,
            active_tab: 0,
            selected_cmd: 0,
            completed: false,
            accepted: None,
        }
    }

    fn active_commands(&self) -> &[&'static CommandMeta] {
        &self.groups[self.active_tab].commands
    }

    fn visible_command_range(&self, height: u16) -> std::ops::Range<usize> {
        let commands = self.active_commands();
        let visible_rows = visible_command_rows(height, commands.len());
        let visible_start = self
            .selected_cmd
            .saturating_add(1)
            .saturating_sub(visible_rows);
        visible_start
            ..visible_start
                .saturating_add(visible_rows)
                .min(commands.len())
    }
}

impl BottomPaneView for HelpView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < MIN_VIEW_HEIGHT {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let sel = Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD);
        // Plain semantic tabs stay aligned across fonts and terminals.
        {
            let mut spans: Vec<Span> = vec![Span::raw("  ")];
            for (i, gd) in self.groups.iter().enumerate() {
                let label = gd.title.to_string();
                if i == self.active_tab {
                    spans.push(Span::styled(label, sel));
                } else {
                    spans.push(Span::styled(label, dim));
                }
                spans.push(Span::raw("  "));
            }
            Widget::render(
                Line::from(spans),
                Rect::new(area.x, area.y, area.width, TAB_ROWS),
                buf,
            );
        }

        // Commands in active group
        let cmds = self.active_commands();
        let visible = self.visible_command_range(area.height);
        let command_y = area.y + TAB_ROWS + COMMAND_SPACER_ROWS;

        for (i, &meta) in cmds
            .iter()
            .enumerate()
            .skip(visible.start)
            .take(visible.len())
        {
            let y = command_y + (i - visible.start) as u16;
            let is_sel = i == self.selected_cmd;

            let cmd_display = if let Some(hint) = meta.arg_hint {
                format!("{} {}", meta.name, hint)
            } else {
                meta.name.to_string()
            };

            let name_w = 30;
            let cmd_display = crate::tui::truncate_ellipsis(&cmd_display, name_w);
            let padded = format!("{:<width$}", cmd_display, width = name_w);
            let desc_budget = (area.width as usize).saturating_sub(4 + name_w);
            let desc: String = meta.description.chars().take(desc_budget).collect();

            let line = if is_sel {
                Line::from(vec![
                    Span::styled("  ", sel),
                    Span::styled(padded, sel),
                    Span::styled(desc, sel),
                ])
            } else {
                Line::from(vec![
                    Span::raw("  "),
                    Span::raw("  "),
                    Span::raw(padded),
                    Span::styled(desc, dim),
                ])
            };
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
        }

        if area.height >= MIN_VIEW_HEIGHT {
            let hint_y = area.bottom() - HINT_ROWS;
            let hint = Line::from(Span::styled(format!("  {}", help_hint()), dim));
            Widget::render(hint, Rect::new(area.x, hint_y, area.width, HINT_ROWS), buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let cmds_h = self.active_commands().len().min(MAX_CMD_ROWS) as u16;
        TAB_ROWS + COMMAND_SPACER_ROWS + cmds_h + HINT_SPACER_ROWS + HINT_ROWS
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let cmd_count = self.active_commands().len();
        match key.code {
            KeyCode::Left => {
                if self.active_tab > 0 {
                    self.active_tab -= 1;
                } else {
                    self.active_tab = self.groups.len() - 1;
                }
                self.selected_cmd = 0;
            }
            KeyCode::Right => {
                self.active_tab = (self.active_tab + 1) % self.groups.len();
                self.selected_cmd = 0;
            }
            KeyCode::Up if cmd_count > 0 => {
                self.selected_cmd = if self.selected_cmd == 0 {
                    cmd_count - 1
                } else {
                    self.selected_cmd - 1
                };
            }
            KeyCode::Down if cmd_count > 0 => {
                self.selected_cmd = (self.selected_cmd + 1) % cmd_count;
            }
            KeyCode::Enter => {
                if let Some(meta) = self.active_commands().get(self.selected_cmd) {
                    self.accepted = Some(meta.name.to_string());
                    self.completed = true;
                }
            }
            KeyCode::Esc => {
                self.completed = true;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: self
                    .accepted
                    .clone()
                    .map(super::view::ViewResult::InsertCommand),
                reopen: None,
            })
        } else {
            None
        }
    }

    fn hint_keys(&self) -> Option<String> {
        Some(help_hint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::bottom_pane::view::ViewResult;

    fn selected_command_line(view: &HelpView, area: Rect) -> String {
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);

        let accent = crate::tui::theme::current().accent;
        let selected_y = (area.y..area.bottom())
            .find(|&y| {
                let cell = &buffer[(area.x, y)];
                cell.fg == accent && cell.modifier.contains(Modifier::BOLD)
            })
            .expect("the selected command is rendered in the visible area");

        (area.x..area.right())
            .map(|x| buffer[(x, selected_y)].symbol())
            .collect()
    }

    #[test]
    fn help_browser_groups_every_tui_root_once() {
        let view = HelpView::new();
        let mut expected: Vec<_> = command_registry::tui_commands()
            .filter(|command| !command.name.contains(' '))
            .map(|command| command.name)
            .collect();
        let mut listed: Vec<_> = view
            .groups
            .iter()
            .skip(1) // Featured is a convenience tab; category tabs are the catalog.
            .flat_map(|group| group.commands.iter().map(|command| command.name))
            .collect();
        expected.sort_unstable();
        listed.sort_unstable();

        assert_eq!(listed.len(), expected.len());
        assert_eq!(listed, expected);
    }

    #[test]
    fn featured_tab_contains_the_curated_primary_actions() {
        let view = HelpView::new();
        let featured: Vec<_> = view.groups[0]
            .commands
            .iter()
            .map(|command| command.name)
            .collect();

        assert_eq!(
            featured,
            [
                "/help", "/model", "/clear", "/history", "/resume", "/stop", "/plan", "/work",
                "/context", "/agent",
            ]
        );
    }

    #[test]
    fn featured_selection_stays_visible_when_short_and_after_resize() {
        let mut view = HelpView::new();
        let featured: Vec<_> = view
            .active_commands()
            .iter()
            .map(|command| command.name)
            .collect();
        let short = Rect::new(0, 0, 80, 8);

        for &command in &featured {
            let selected_line = selected_command_line(&view, short);
            assert!(
                selected_line.contains(command),
                "selected command {command} should remain visible in an 80x8 view; got:\n{selected_line}"
            );
            view.handle_key(crate::tui::testing::keys::key(KeyCode::Down));
        }

        assert_eq!(
            view.selected_cmd, 0,
            "Down wraps to the first featured action"
        );
        assert!(selected_command_line(&view, short).contains(featured[0]));

        for _ in 0..6 {
            view.handle_key(crate::tui::testing::keys::key(KeyCode::Down));
        }
        assert_eq!(view.selected_cmd, 6);
        assert!(selected_command_line(&view, short).contains(featured[6]));

        let resized = Rect::new(0, 0, 80, 14);
        assert!(selected_command_line(&view, resized).contains(featured[6]));

        view.handle_key(crate::tui::testing::keys::key(KeyCode::Enter));
        assert_eq!(
            view.completion().and_then(|completion| completion.result),
            Some(ViewResult::InsertCommand(featured[6].to_string()))
        );
    }
}
