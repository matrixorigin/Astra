//! Task-board visibility state machine.
//!
//! The board can be shown or hidden by two forces:
//!
//! * **User intent** — Ctrl+T flips the board explicitly. Captured as
//!   `user_pin: Option<bool>` where `None` means "user hasn't touched
//!   this; follow automatic behaviour" and `Some(true|false)` pins the
//!   choice until the user toggles again or the board empties out.
//! * **Automatic heuristics** — observer sees tasks appear / the last
//!   task completes. Only take effect when `user_pin == None`.
//!
//! Keeping the decision in one pure function means the event loop
//! doesn't have to remember which branch wins in each state; it just
//! feeds the inputs in and reads out the new `expanded` flag.
//!
//! This fixes two prior bugs that confused users:
//! 1. Ctrl+T on an active cell was sometimes swallowed by modal-view
//!    guards, so the key felt unreliable. (Now routed through the
//!    pure resolver — guards happen at the keymap layer above.)
//! 2. Auto-open/auto-close fought with manual toggle — a user pinning
//!    the board open would see it slam shut the next time the hide
//!    timer fired. The pin now overrides automatic hide/open.

/// Compute the new `expanded` flag for the task board.
///
/// - `prev_expanded`: what the board was doing last tick.
/// - `user_pin`:
///     * `None` — user hasn't expressed an opinion; automatic rules run.
///     * `Some(true)` — user hit Ctrl+T to open; stay open even if the
///       auto-hide timer fires.
///     * `Some(false)` — user hit Ctrl+T to close; stay closed even if
///       new tasks arrive (until the list empties, which resets the pin).
/// - `has_tasks`: observer snapshot is non-empty (a live session has rows).
/// - `board_hidden`: observer's internal hidden flag (all-completed idle timer fired).
///
/// Also returns `reset_pin: bool` — caller should clear
/// `user_pin` back to `None` when `true`, i.e. when the list emptied
/// out so the next auto-open can fire from a clean slate.
pub(crate) fn resolve_board_visibility(
    prev_expanded: bool,
    user_pin: Option<bool>,
    has_tasks: bool,
    board_hidden: bool,
) -> (bool, bool) {
    // Empty list → hide, and reset the pin so the next session with
    // tasks is back in "auto" mode.
    if !has_tasks {
        return (false, user_pin.is_some());
    }
    // User has an explicit choice → honour it, regardless of what
    // the observer's internal hide timer thinks.
    if let Some(pin) = user_pin {
        return (pin, false);
    }
    // Observer's auto-hide timer says "collapse" — respect it, but
    // only when the user hasn't pinned.
    if board_hidden {
        return (false, false);
    }
    // Default = collapsed (one-line summary). Earlier behaviour
    // auto-expanded the full panel as soon as a task appeared, which
    // ate ~8 rows of streaming space on every multi-step turn. Now
    // the user opts in via Ctrl+T (which sets `user_pin = Some(true)`
    // and short-circuits above) — the auto path stays compact.
    (prev_expanded, false)
}

#[cfg(test)]
mod tests {
    use super::resolve_board_visibility;

    #[test]
    fn empty_list_collapses_board() {
        let (expanded, reset) = resolve_board_visibility(true, None, false, false);
        assert!(!expanded, "empty list must collapse");
        assert!(!reset, "pin was already None — nothing to reset");
    }

    #[test]
    fn empty_list_resets_user_pin_for_next_session() {
        // User had pinned open; every task finished & list went empty.
        // The pin should clear so the next cycle's auto-open works.
        let (expanded, reset) = resolve_board_visibility(true, Some(true), false, false);
        assert!(!expanded);
        assert!(reset, "empty list must reset the user pin");
    }

    #[test]
    fn user_pin_open_overrides_auto_hide() {
        // All tasks completed, idle timer says hide. User-pin=open
        // must keep the board visible — this is the "pin it open
        // while I review the run" scenario.
        let (expanded, reset) = resolve_board_visibility(true, Some(true), true, true);
        assert!(expanded, "user pin must override auto-hide: {expanded}");
        assert!(!reset);
    }

    #[test]
    fn user_pin_closed_overrides_auto_open() {
        // User explicitly hit Ctrl+T to close. A new task arriving
        // must NOT re-pop the board — that's the "don't fight me"
        // rule that separates astra from claude-code's auto surface.
        let (expanded, _) = resolve_board_visibility(false, Some(false), true, false);
        assert!(!expanded, "user pin closed must stay closed");
    }

    #[test]
    fn first_tasks_appearing_stays_collapsed_until_user_pins() {
        // Default behaviour: a brand-new task list does NOT auto-expand
        // the full panel — it stays as a one-line summary above the
        // composer. The user opts in via Ctrl+T (Some(true) pin).
        let (expanded, reset) = resolve_board_visibility(false, None, true, false);
        assert!(
            !expanded,
            "default should stay collapsed; full-panel mode is opt-in"
        );
        assert!(!reset);
    }

    #[test]
    fn auto_hide_respected_when_unpinned() {
        // Unpinned, observer's all-completed idle timer fired.
        let (expanded, _) = resolve_board_visibility(true, None, true, true);
        assert!(!expanded);
    }

    #[test]
    fn already_open_stays_open_with_ongoing_tasks() {
        // Board is already showing; new tick has tasks but isn't
        // hidden. Nothing should flap.
        let (expanded, _) = resolve_board_visibility(true, None, true, false);
        assert!(expanded);
    }

    #[test]
    fn unpinned_closed_without_hide_flag_stays_collapsed_for_new_tasks() {
        // Inverted from the prior auto-open default: collapsed boards
        // stay collapsed when new tasks land. The user explicitly
        // expands via Ctrl+T → user_pin = Some(true).
        let (expanded, _) = resolve_board_visibility(false, None, true, false);
        assert!(!expanded);
    }

    #[test]
    fn user_pin_open_survives_multiple_ticks() {
        // Simulate 5 ticks: pin open, tasks ongoing, no flap.
        let mut exp = true;
        for _ in 0..5 {
            let (next, reset) = resolve_board_visibility(exp, Some(true), true, false);
            assert!(next && !reset, "pinned board must stay open every tick");
            exp = next;
        }
    }
}
