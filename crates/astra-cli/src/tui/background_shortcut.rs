pub(crate) fn ctrl_b_background_shortcut() -> &'static str {
    ctrl_b_background_shortcut_for_tmux(is_tmux_session())
}

pub(crate) fn background_task_open_hint() -> &'static str {
    "Shift+↓ manage"
}

pub(crate) fn agent_workbench_open_hint() -> &'static str {
    "Ctrl+G agents"
}

pub(crate) fn ctrl_b_background_shortcut_for_tmux(is_tmux: bool) -> &'static str {
    if is_tmux {
        "Ctrl+B Ctrl+B (twice)"
    } else {
        "Ctrl+B"
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
    fn background_task_open_hint_uses_navigation_manage_shortcut() {
        assert_eq!(background_task_open_hint(), "Shift+↓ manage");
    }

    #[test]
    fn agent_workbench_hint_names_the_agent_surface() {
        assert_eq!(agent_workbench_open_hint(), "Ctrl+G agents");
    }
}
