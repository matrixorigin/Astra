use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};
use crate::command_registry::{self, CommandGroup, CommandMeta};

const MAX_CMD_ROWS: usize = 10;

struct GroupData {
    group: CommandGroup,
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
        let groups: Vec<GroupData> = CommandGroup::ALL
            .iter()
            .filter_map(|&g| {
                let cmds: Vec<&'static CommandMeta> = command_registry::commands_by_group(g)
                    .filter(|m| !m.is_alias && !m.name.contains(' '))
                    .collect();
                if cmds.is_empty() {
                    None
                } else {
                    Some(GroupData {
                        group: g,
                        commands: cmds,
                    })
                }
            })
            .collect();
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

    fn active_group(&self) -> &GroupData {
        &self.groups[self.active_tab]
    }
}

impl BottomPaneView for HelpView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 4 {
            return;
        }

        let dim = Style::default().fg(Color::DarkGray);
        let sel = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut y = area.y;

        // Tab bar: ⚡Core  📂Workspace  🔭Observability ...
        {
            let mut spans: Vec<Span> = vec![Span::raw("  ")];
            for (i, gd) in self.groups.iter().enumerate() {
                let label = format!("{}{}", gd.group.icon(), gd.group.title());
                if i == self.active_tab {
                    spans.push(Span::styled(label, sel));
                } else {
                    spans.push(Span::styled(label, dim));
                }
                spans.push(Span::raw("  "));
            }
            Widget::render(Line::from(spans), Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        // Blank line
        if y >= area.bottom() {
            return;
        }
        y += 1;

        // Commands in active group
        let cmds = self.active_commands();
        let visible_start = if self.selected_cmd >= MAX_CMD_ROWS {
            self.selected_cmd - MAX_CMD_ROWS + 1
        } else {
            0
        };
        let visible_end = (visible_start + MAX_CMD_ROWS).min(cmds.len());

        for (i, &meta) in cmds
            .iter()
            .enumerate()
            .skip(visible_start)
            .take(visible_end - visible_start)
        {
            if y >= area.bottom() {
                return;
            }
            let is_sel = i == self.selected_cmd;

            let cmd_display = if let Some(hint) = meta.arg_hint {
                format!("{} {}", meta.name, hint)
            } else {
                meta.name.to_string()
            };

            let name_w = 30;
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
            y += 1;
        }

        // Blank + hint
        if y < area.bottom() {
            y += 1;
        }
        if y < area.bottom() {
            let hint = Line::from(Span::styled(
                "  ←/→ switch group  ↑/↓ browse  Enter select  Esc close",
                dim,
            ));
            Widget::render(hint, Rect::new(area.x, y, area.width, 1), buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let tab_h = 1;
        let blank = 1;
        let cmds_h = self.active_commands().len().min(MAX_CMD_ROWS) as u16;
        let hint_h = 2; // blank + hint
        tab_h + blank + cmds_h + hint_h
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
                result: self.accepted.clone(),
                reopen: None,
            })
        } else {
            None
        }
    }
}
