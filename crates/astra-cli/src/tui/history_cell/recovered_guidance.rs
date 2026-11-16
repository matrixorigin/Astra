//! Local receipt for a message whose applied status was checked later.

use std::{any::Any, sync::Arc};

use ratatui::text::Line;

use super::{HistoryCell, system::SystemCell};
use crate::tui::bottom_pane::RootIntentScope;

/// The full message stays in local history; the terminal receives a short
/// receipt. It is not a new user turn or a durable transcript event.
#[derive(Debug)]
pub(crate) struct RecoveredGuidanceCell {
    pub(crate) scope: Arc<RootIntentScope>,
    pub(crate) text: String,
    notice: SystemCell,
}

impl RecoveredGuidanceCell {
    pub(crate) fn new(scope: Arc<RootIntentScope>, text: String) -> Self {
        let preview = guidance_preview(&text);
        let notice = SystemCell::info(format!(
            "Earlier message applied to its original task: {preview} · Alt+R view/copy before switching sessions"
        ));
        Self {
            scope,
            text,
            notice,
        }
    }
}

pub(crate) fn guidance_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview: String = chars
        .by_ref()
        .take(64)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

impl HistoryCell for RecoveredGuidanceCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.scope.owner_can_view() {
            self.notice.display_lines(width)
        } else {
            SystemCell::info("Earlier message applied to its original task.").display_lines(width)
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
