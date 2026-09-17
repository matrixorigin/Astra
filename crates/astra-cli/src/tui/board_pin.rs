//! Task-board visibility state machine.
//!
//! The board can be shown or hidden by two forces:
//!
//! * **User intent** — Ctrl+T flips the board explicitly. Captured as
//!   `user_pin: Option<bool>` where `None` means "user hasn't touched
//!   this; follow the work-aware baseline" and `Some(true|false)` pins the
//!   choice until the user toggles again or the board empties out.
//! * **Automatic baseline** — open work expands the board so the current
//!   plan is visible while it is being executed. Terminal history stays
//!   reachable through Ctrl+T without occupying the live viewport.
//!
//! Keeping the decision in one pure function means the event loop
//! doesn't have to remember which branch wins in each state; it just
//! feeds the inputs in and reads out the new `expanded` flag.
//!
//! This fixes two prior bugs that confused users:
//! 1. Ctrl+T on an active cell was sometimes swallowed by modal-view
//!    guards, so the key felt unreliable. (Now routed through the
//!    pure resolver — guards happen at the keymap layer above.)
//! 2. Automatic state changes fought with manual toggle — a user's pin now
//!    remains authoritative until the source becomes empty.

/// Compute the new `expanded` flag for the task board.
///
/// - `user_pin`:
///   * `None` — user hasn't expressed an opinion; the work-aware baseline
///     applies.
///   * `Some(true)` — user hit Ctrl+T to open; stay open.
///   * `Some(false)` — user hit Ctrl+T to close; stay closed even if
///     new tasks arrive (until the list empties, which resets the pin).
/// - `has_tasks`: observer snapshot is non-empty (a live session has rows).
/// - `has_open_work`: the canonical projection still has a task that needs
///   execution or an explicitly reported outcome.
///
/// Also returns `reset_pin: bool` — caller should clear
/// `user_pin` back to `None` when `true`, i.e. when the list emptied
/// out so the next auto-open can fire from a clean slate.
pub(crate) fn resolve_board_visibility(
    user_pin: Option<bool>,
    has_tasks: bool,
    has_open_work: bool,
) -> (bool, bool) {
    // Empty list → hide, and reset the pin so the next session with
    // tasks is back in "auto" mode.
    if !has_tasks {
        return (false, user_pin.is_some());
    }
    // User has an explicit choice → honour it.
    if let Some(pin) = user_pin {
        return (pin, false);
    }
    // A plan is most useful while it can still guide or explain execution.
    // Do not make the user discover it through a keyboard shortcut in the
    // middle of a multi-step turn. Once every row is terminal, collapse it
    // unless the user explicitly pinned it open for review.
    (has_open_work, false)
}

#[cfg(test)]
mod tests {
    use super::resolve_board_visibility;

    #[test]
    fn empty_list_collapses_board() {
        let (expanded, reset) = resolve_board_visibility(None, false, false);
        assert!(!expanded, "empty list must collapse");
        assert!(!reset, "pin was already None — nothing to reset");
    }

    #[test]
    fn empty_list_resets_user_pin_for_next_session() {
        // User had pinned open; every task finished & list went empty.
        // The pin should clear so the next cycle's auto-open works.
        let (expanded, reset) = resolve_board_visibility(Some(true), false, false);
        assert!(!expanded);
        assert!(reset, "empty list must reset the user pin");
    }

    #[test]
    fn user_pin_open_keeps_terminal_work_visible() {
        let (expanded, reset) = resolve_board_visibility(Some(true), true, false);
        assert!(expanded, "user pin must keep the board open: {expanded}");
        assert!(!reset);
    }

    #[test]
    fn user_pin_closed_overrides_auto_open() {
        // User explicitly hit Ctrl+T to close. A new task arriving
        // must NOT re-pop the board — that's the "don't fight me"
        // rule that separates astra from reference-agent's auto surface.
        let (expanded, _) = resolve_board_visibility(Some(false), true, true);
        assert!(!expanded, "user pin closed must stay closed");
    }

    #[test]
    fn active_work_auto_expands_without_a_user_pin() {
        let (expanded, reset) = resolve_board_visibility(None, true, true);
        assert!(
            expanded,
            "active Work must be visible without requiring a Ctrl+T discovery step"
        );
        assert!(!reset);
    }

    #[test]
    fn terminal_history_auto_collapses_without_a_user_pin() {
        let (expanded, reset) = resolve_board_visibility(None, true, false);
        assert!(
            !expanded,
            "terminal history should not occupy the live viewport"
        );
        assert!(!reset);
    }

    #[test]
    fn user_pin_open_survives_multiple_ticks() {
        // Simulate 5 ticks: pin open, tasks ongoing, no flap.
        for _ in 0..5 {
            let (next, reset) = resolve_board_visibility(Some(true), true, true);
            assert!(next && !reset, "pinned board must stay open every tick");
        }
    }
}
