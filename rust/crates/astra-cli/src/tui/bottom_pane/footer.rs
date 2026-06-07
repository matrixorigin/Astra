//! Thin compatibility shim over [`crate::tui::status_line::StatusLine`].
//!
//! The rendered bottom strip is now composed by [`StatusLine`] from a
//! pure [`StatusContext`]; this struct remains as the mutable container
//! the event loop writes into, to avoid churn across call sites.

use std::time::Duration;

use ratatui::{buffer::Buffer, layout::Rect};

use crate::cli::permission_manager::{PermissionMode, PermissionModeMirror};
use crate::tui::status_line::{StatusContext, StatusLine};

pub(crate) struct Footer {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub token_usage: Option<String>,
    pub cwd: Option<String>,
    pub is_turn_active: bool,
    pub permission_mode: Option<PermissionMode>,
    pub git_branch: Option<String>,
    pub token_budget: Option<(u64, u64)>,
    pub current_objective: Option<String>,
    pub turn_elapsed: Option<Duration>,
    pub pending_approvals: usize,
    pub task_counts: Option<(usize, usize)>,
    pub task_board_expanded: bool,
    /// `(running, stalled)` snapshot of the BackgroundTaskRegistry.
    /// Updated by the TUI event-loop tick; rendered as the `BG: …`
    /// chip on the status line. `None` keeps the chip hidden.
    pub bg_task_counts: Option<(usize, usize)>,
    /// Lock-free mirror of the current permission mode. When set,
    /// `to_context()` reads the live mode from this mirror on every
    /// render instead of relying on the cached `permission_mode`
    /// field. Eliminates the ~50 ms staleness window between a mode
    /// change (e.g. `/plan`, `/auto`, exit_plan_mode) and the next
    /// event-loop tick that calls `refresh_footer_from_state`.
    mode_mirror: Option<PermissionModeMirror>,
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
            git_branch: detect_git_branch(),
            token_budget: None,
            current_objective: None,
            turn_elapsed: None,
            pending_approvals: 0,
            task_counts: None,
            task_board_expanded: false,
            bg_task_counts: None,
            mode_mirror: None,
        }
    }

    /// Re-probe cwd + git branch. Cheap (gix discover + a single ref
    /// read; cwd is a syscall). Called on every turn boundary so the
    /// status line stays near-real-time without a background watcher.
    pub fn refresh_env(&mut self) {
        self.cwd = current_cwd_display();
        self.git_branch = detect_git_branch();
    }

    /// Install a lock-free mirror so every frame reads the *current*
    /// permission mode directly from the atomic state rather than a
    /// cached copy that may be up to one event-loop tick stale.
    pub fn set_mode_mirror(&mut self, mirror: PermissionModeMirror) {
        self.mode_mirror = Some(mirror);
    }

    /// Resolve the live permission mode: prefer the lock-free mirror
    /// when available; otherwise fall back to the cached field (for
    /// tests and early-init renders before the mirror is wired).
    fn live_mode(&self) -> PermissionMode {
        self.mode_mirror
            .as_ref()
            .map(|m| m.current())
            .unwrap_or(self.permission_mode.unwrap_or_default())
    }

    fn to_context(&self) -> StatusContext {
        StatusContext {
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            token_budget: self.token_budget,
            current_objective: self.current_objective.clone(),
            turn_elapsed: self.turn_elapsed,
            permission_mode: self.live_mode(),
            turn_active: self.is_turn_active,
            session_id: self.session_id.clone(),
            cost_usd: None,
            // Prefer the cached field refreshed by `refresh_env()`,
            // but fall back to a direct probe if the footer has not
            // been initialized yet (useful for early renders/tests).
            git_branch: self.git_branch.clone().or_else(detect_git_branch),
            pending_approvals: self.pending_approvals,
            task_counts: self.task_counts,
            task_board_expanded: self.task_board_expanded,
            bg_task_counts: self.bg_task_counts,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let panel = crate::tui::style::footer_surface_style();
        for y in area.y..area.y + area.height {
            buf.set_string(area.x, y, " ".repeat(area.width as usize), panel);
        }
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

#[cfg(test)]
mod tests {
    use super::Footer;
    use std::time::Duration;

    #[test]
    fn footer_passes_objective_and_elapsed_to_status_line() {
        let mut footer = Footer::new();
        footer.current_objective = Some("Running bash".to_string());
        footer.turn_elapsed = Some(Duration::from_secs(16));
        let ctx = footer.to_context();
        assert_eq!(ctx.current_objective.as_deref(), Some("Running bash"));
        assert_eq!(ctx.turn_elapsed, Some(Duration::from_secs(16)));
    }
}
