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
    pub pending_approvals: usize,
    pub task_counts: Option<(usize, usize)>,
    pub task_board_expanded: bool,
    /// `(running, stalled)` snapshot of the BackgroundTaskRegistry.
    /// Updated by the TUI event-loop tick; rendered as the `BG: …`
    /// chip on the status line. `None` keeps the chip hidden.
    pub bg_task_counts: Option<(usize, usize)>,
}

impl Footer {
    pub fn new() -> Self {
        Self {
            model: None,
            session_id: None,
            token_usage: None,
            cwd: current_cwd_display(),
            is_turn_active: false,
            permission_mode: None,
            cost_usd: None,
            git_branch: detect_git_branch(),
            token_budget: None,
            pending_approvals: 0,
            task_counts: None,
            task_board_expanded: false,
            bg_task_counts: None,
        }
    }

    /// Re-probe cwd + git branch. Cheap (gix discover + a single ref
    /// read; cwd is a syscall). Called on every turn boundary so the
    /// status line stays near-real-time without a background watcher.
    pub fn refresh_env(&mut self) {
        self.cwd = current_cwd_display();
        self.git_branch = detect_git_branch();
    }

    fn permission_mode_enum(&self) -> PermissionMode {
        match self.permission_mode.as_deref() {
            Some("auto") => PermissionMode::Auto,
            Some("plan") => PermissionMode::Plan,
            Some("accept_edits") => PermissionMode::AcceptEdits,
            Some("deny") => PermissionMode::Deny,
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
            pending_approvals: self.pending_approvals,
            task_counts: self.task_counts,
            task_board_expanded: self.task_board_expanded,
            bg_task_counts: self.bg_task_counts,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        StatusLine::from_context(&self.to_context()).render(area, buf);
    }
}

/// Home-shortened cwd for the status line (`~/foo/bar`). Falls back
/// to the absolute path when cwd isn't under `$HOME`. `None` only on
/// `getcwd` failure (e.g. the directory was deleted).
fn current_cwd_display() -> Option<String> {
    let p = std::env::current_dir().ok()?;
    let home = dirs::home_dir();
    Some(match home {
        Some(h) if p.starts_with(&h) => {
            format!("~/{}", p.strip_prefix(&h).unwrap_or(&p).display())
        }
        _ => p.display().to_string(),
    })
}

/// One-shot git branch lookup via `gix`. Returns the short branch
/// name on a normal HEAD, a parenthesized short SHA on detached HEAD
/// (covers `git bisect` / `git checkout <sha>`), and `None` for
/// non-git cwds or I/O errors — the status line then shows just the
/// cwd.
///
/// Delegates to the process-wide cached implementation so the footer
/// (which redraws every frame) doesn't spawn a `gix::discover` per
/// render. See `crate::git_branch_cache`.
fn detect_git_branch() -> Option<String> {
    crate::git_branch_cache::detect_git_branch_cached()
}
