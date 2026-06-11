pub(crate) fn ctrl_b_background_shortcut() -> &'static str {
    ctrl_b_background_shortcut_for_tmux(is_tmux_session())
}

pub(crate) fn background_task_open_hint() -> String {
    format!(
        "{} open",
        background_task_open_shortcuts_for_tmux(is_tmux_session())
    )
}

pub(crate) fn ctrl_b_background_shortcut_for_tmux(is_tmux: bool) -> &'static str {
    if is_tmux {
        "Ctrl+B Ctrl+B (twice)"
    } else {
        "Ctrl+B"
    }
}

pub(crate) fn background_task_open_shortcuts_for_tmux(is_tmux: bool) -> &'static str {
    if is_tmux {
        "Ctrl+B Ctrl+B/Ctrl+T"
    } else {
        "Ctrl+B/Ctrl+T"
    }
}

fn is_tmux_session() -> bool {
    std::env::var_os("TMUX").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_background_shortcut_matches_tmux_prefix_behavior() {
        assert_eq!(ctrl_b_background_shortcut_for_tmux(false), "Ctrl+B");
        assert_eq!(
            ctrl_b_background_shortcut_for_tmux(true),
            "Ctrl+B Ctrl+B (twice)"
        );
    }

    #[test]
    fn background_task_open_shortcuts_stay_compact_in_tmux() {
        assert_eq!(
            background_task_open_shortcuts_for_tmux(false),
            "Ctrl+B/Ctrl+T"
        );
        assert_eq!(
            background_task_open_shortcuts_for_tmux(true),
            "Ctrl+B Ctrl+B/Ctrl+T"
        );
    }
}
