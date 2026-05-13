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
            pending_approvals: self.pending_approvals,
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
fn detect_git_branch() -> Option<String> {
    let repo = gix::discover(std::env::current_dir().ok()?).ok()?;
    let head = repo.head().ok()?;
    if let Some(name) = head.referent_name() {
        return Some(name.shorten().to_string());
    }
    // Detached HEAD: show abbreviated commit id.
    let id = head.id()?;
    let hex = id.to_hex_with_len(7).to_string();
    Some(format!("({hex})"))
}
