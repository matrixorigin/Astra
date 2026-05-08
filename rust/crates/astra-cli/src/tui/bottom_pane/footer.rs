//! Thin compatibility shim over [`crate::tui::status_line::StatusLine`].
//!
//! The rendered bottom strip is now composed by [`StatusLine`] from a
//! pure [`StatusContext`]; this struct remains as the mutable container
//! the event loop writes into, to avoid churn across call sites.

use ratatui::{buffer::Buffer, layout::Rect};

use crate::tui::status_line::{PermissionMode, StatusContext, StatusLine};

pub(crate) struct Footer {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub token_usage: Option<String>,
    pub cwd: Option<String>,
    pub is_turn_active: bool,
    pub permission_mode: Option<String>,
    pub cost_usd: Option<f64>,
    pub git_branch: Option<String>,
    pub token_budget: Option<(u64, u64)>,
}

impl Footer {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().ok().map(|p| {
            let home = dirs::home_dir();
            match home {
                Some(h) if p.starts_with(&h) => {
                    format!("~/{}", p.strip_prefix(&h).unwrap_or(&p).display())
                }
                _ => p.display().to_string(),
            }
        });
        Self {
            model: None,
            session_id: None,
            token_usage: None,
            cwd,
            is_turn_active: false,
            permission_mode: None,
            cost_usd: None,
            git_branch: None,
            token_budget: None,
        }
    }

    fn permission_mode_enum(&self) -> PermissionMode {
        match self.permission_mode.as_deref() {
            Some("auto") => PermissionMode::Auto,
            Some("deny") => PermissionMode::Deny,
            Some("bypass") => PermissionMode::Bypass,
            _ => PermissionMode::Ask,
        }
    }

    fn to_context(&self) -> StatusContext {
        StatusContext {
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            token_budget: self.token_budget,
            permission_mode: self.permission_mode_enum(),
            turn_active: self.is_turn_active,
            session_id: self.session_id.clone(),
            cost_usd: self.cost_usd,
            git_branch: self.git_branch.clone(),
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        StatusLine::from_context(&self.to_context()).render(area, buf);
    }
}
