use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};

use super::ChatCell;

#[derive(Debug)]
pub(crate) struct SystemChatCell {
    pub message: String,
    pub level: SystemLevel,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SystemLevel {
    Info,
    #[allow(dead_code)]
    Warning,
    #[allow(dead_code)]
    Error,
}

impl SystemChatCell {
    pub fn info(message: String) -> Self {
        Self {
            message,
            level: SystemLevel::Info,
        }
    }

    #[allow(dead_code)]
    pub fn warning(message: String) -> Self {
        Self {
            message,
            level: SystemLevel::Warning,
        }
    }

    #[allow(dead_code)]
    pub fn error(message: String) -> Self {
        Self {
            message,
            level: SystemLevel::Error,
        }
    }
}

impl ChatCell for SystemChatCell {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any_ref(&self) -> &dyn std::any::Any {
        self
    }
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let style = match self.level {
            SystemLevel::Info => Style::default().dim(),
            SystemLevel::Warning => Style::default().yellow(),
            SystemLevel::Error => Style::default().red(),
        };
        self.message
            .lines()
            .map(|l| Line::from(Span::styled(format!("  {l}"), style)))
            .collect()
    }
}
