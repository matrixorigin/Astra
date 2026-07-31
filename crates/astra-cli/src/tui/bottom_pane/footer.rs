//! Thin compatibility shim over [`crate::tui::status_line::StatusLine`].
//!
//! The rendered bottom strip is now composed by [`StatusLine`] from a
//! pure [`StatusContext`]; this struct remains as the mutable container
//! the event loop writes into, to avoid churn across call sites.

use std::time::Duration;

use ratatui::{buffer::Buffer, layout::Rect};

use crate::cli::permission_manager::{PermissionMode, PermissionModeMirror};
use crate::tui::status_line::{BackgroundTaskCounts, StatusContext, StatusLine};
use astra_turn_types::{ContextWindowUsage, ContextWindowUsageSource};

pub(crate) struct Footer {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub token_usage: Option<String>,
    pub cwd: Option<String>,
    pub is_turn_active: bool,
    pub permission_mode: Option<PermissionMode>,
    pub git_branch: Option<String>,
    /// Current request's usable context-window occupancy. This deliberately
    /// does not use cumulative session/billing token totals.
    pub context_window: Option<ContextWindowUsage>,
    pub raw_context_window_tokens: Option<u64>,
    pub request_token_usage: Option<astra_turn_types::RequestTokenUsage>,
    pending_context_window_policy: Option<(u64, u64)>,
    context_window_is_previous: bool,
    /// Client-owned portion of the current request. The runtime adds the
    /// exact system-prompt count through a typed SSE signal.
    context_window_non_system_tokens: Option<(u64, u64)>,
    last_system_prompt_tokens: Option<u32>,
    pub current_objective: Option<String>,
    pub turn_elapsed: Option<Duration>,
    pub pending_approvals: usize,
    pub task_counts: Option<(usize, usize)>,
    pub task_board_expanded: bool,
    /// Snapshot of BackgroundTaskRegistry states that need user
    /// visibility. Updated by the TUI event-loop tick. `None` keeps
    /// the chip hidden.
    pub bg_task_counts: Option<BackgroundTaskCounts>,
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
            context_window: None,
            raw_context_window_tokens: None,
            request_token_usage: None,
            pending_context_window_policy: None,
            context_window_is_previous: false,
            context_window_non_system_tokens: None,
            last_system_prompt_tokens: None,
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
            context_window: self.context_window,
            raw_context_window_tokens: self.raw_context_window_tokens,
            context_window_is_previous: self.context_window_is_previous,
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

    /// Start a new request-level estimate. Preserve the preceding system
    /// prompt estimate until this request's `context_meta` arrives, avoiding a
    /// distracting drop between agentic tool rounds.
    pub fn begin_context_window_estimate(&mut self, usage: ContextWindowUsage) {
        let (raw_window_tokens, usable_input_tokens) = self
            .pending_context_window_policy
            .take()
            .map(|(raw, usable)| (Some(raw), usable))
            .unwrap_or((self.raw_context_window_tokens, usage.limit_tokens));
        if usable_input_tokens == 0 {
            return;
        }
        self.raw_context_window_tokens = raw_window_tokens;
        self.context_window_non_system_tokens = Some((usage.used_tokens, usable_input_tokens));
        let system_tokens = u64::from(self.last_system_prompt_tokens.unwrap_or(0));
        self.context_window = Some(ContextWindowUsage::estimated(
            usage.used_tokens.saturating_add(system_tokens),
            usable_input_tokens,
        ));
        self.context_window_is_previous = false;
    }

    pub fn set_context_window_policy(&mut self, raw_tokens: u64, usable_tokens: u64) {
        if raw_tokens == 0 || usable_tokens == 0 || usable_tokens > raw_tokens {
            return;
        }
        self.pending_context_window_policy = Some((raw_tokens, usable_tokens));
    }

    pub fn set_request_token_usage(&mut self, usage: astra_turn_types::RequestTokenUsage) {
        self.request_token_usage = Some(usage);
    }

    /// A new model request has started. Retain the preceding request as
    /// explicitly stale evidence until the new assembly estimate arrives;
    /// replacing it with an empty gap made the footer look unreliable and
    /// conveyed less truth than the client actually knew.
    pub fn clear_context_window_for_new_request(&mut self) {
        self.context_window_is_previous = self.context_window.is_some();
        self.context_window_non_system_tokens = None;
        self.request_token_usage = None;
        self.pending_context_window_policy = None;
    }

    pub fn context_window_is_previous(&self) -> bool {
        self.context_window_is_previous
    }

    /// Incorporate the runtime's exact system-prompt assembly measurement.
    pub fn set_context_system_prompt_tokens(&mut self, system_tokens: u32) {
        self.last_system_prompt_tokens = Some(system_tokens);
        let Some((non_system_tokens, limit_tokens)) = self.context_window_non_system_tokens else {
            return;
        };
        self.context_window = Some(ContextWindowUsage::estimated(
            non_system_tokens.saturating_add(u64::from(system_tokens)),
            limit_tokens,
        ));
    }

    /// Replace an estimate with the provider-confirmed request input count.
    pub fn set_context_window_measured(&mut self, used_tokens: u64) {
        let Some(current) = self.context_window else {
            return;
        };
        self.context_window = Some(ContextWindowUsage::provider_reported(
            used_tokens,
            current.limit_tokens,
        ));
        self.context_window_is_previous = false;
    }

    /// Restore a completed request's context value without overriding a live
    /// request currently being assembled in this TUI instance.
    pub fn restore_context_window(&mut self, usage: ContextWindowUsage) {
        if (self.context_window.is_none() || self.context_window_is_previous)
            && usage.limit_tokens > 0
        {
            self.context_window = Some(usage);
            self.context_window_is_previous = false;
            if usage.source == ContextWindowUsageSource::Estimated {
                self.context_window_non_system_tokens = None;
            }
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
    use astra_turn_types::{ContextWindowUsage, ContextWindowUsageSource};
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

    #[test]
    fn context_window_replaces_estimate_with_provider_measurement() {
        let mut footer = Footer::new();
        footer.begin_context_window_estimate(ContextWindowUsage::estimated(5_000, 160_000));
        footer.set_context_system_prompt_tokens(9_000);
        assert_eq!(
            footer.to_context().context_window,
            Some(ContextWindowUsage::estimated(14_000, 160_000))
        );

        footer.set_context_window_measured(17_250);
        assert_eq!(
            footer.to_context().context_window,
            Some(ContextWindowUsage::provider_reported(17_250, 160_000))
        );

        footer.clear_context_window_for_new_request();
        let previous = footer.to_context();
        assert_eq!(
            previous.context_window,
            Some(ContextWindowUsage::provider_reported(17_250, 160_000))
        );
        assert!(previous.context_window_is_previous);

        footer.begin_context_window_estimate(ContextWindowUsage::estimated(8_000, 160_000));
        let next_context = footer.to_context();
        let next = next_context.context_window.expect("new estimate");
        assert_eq!(
            next.used_tokens, 17_000,
            "prior system count is a bridge only"
        );
        assert_eq!(next.source, ContextWindowUsageSource::Estimated);
        assert!(!next_context.context_window_is_previous);
    }

    #[test]
    fn context_policy_and_request_lanes_remain_separate_from_session_totals() {
        let mut footer = Footer::new();
        footer.set_context_window_policy(1_000_000, 910_000);
        footer.begin_context_window_estimate(ContextWindowUsage::estimated(700_000, 910_000));
        footer.set_request_token_usage(astra_turn_types::RequestTokenUsage {
            fresh_input_tokens: 100_000,
            cache_read_tokens: 590_000,
            cache_creation_tokens: 10_000,
            output_tokens: 4_000,
        });

        let context = footer.to_context();
        assert_eq!(context.raw_context_window_tokens, Some(1_000_000));
        assert_eq!(
            context.context_window,
            Some(ContextWindowUsage::estimated(700_000, 910_000))
        );
        assert_eq!(
            footer.request_token_usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 100_000,
                cache_read_tokens: 590_000,
                cache_creation_tokens: 10_000,
                output_tokens: 4_000,
            })
        );

        footer.clear_context_window_for_new_request();
        assert!(footer.request_token_usage.is_none());
        assert_eq!(footer.raw_context_window_tokens, Some(1_000_000));

        footer.set_context_window_policy(200_000, 180_000);
        let previous = footer.to_context();
        assert_eq!(
            previous.context_window,
            Some(ContextWindowUsage::estimated(700_000, 910_000)),
            "the next request's policy must not relabel stale evidence from the previous request"
        );
        assert_eq!(previous.raw_context_window_tokens, Some(1_000_000));

        footer.begin_context_window_estimate(ContextWindowUsage::estimated(12_000, 123_456));
        let next = footer.to_context();
        assert_eq!(
            next.context_window,
            Some(ContextWindowUsage::estimated(12_000, 180_000))
        );
        assert_eq!(next.raw_context_window_tokens, Some(200_000));
    }
}
