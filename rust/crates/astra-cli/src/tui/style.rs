use super::color::{blend, is_light};
use super::terminal_palette::{best_color, default_bg};
use ratatui::style::{Color, Style};

pub(crate) fn user_message_style() -> Style {
    // Previously: if the terminal-bg query failed (crossterm 0.28
    // removed it), we returned `Style::default()` and the user turn
    // had no tint at all — visually indistinguishable from assistant
    // cells. Now fall back to the theme's `selected_bg`, which is a
    // concrete color under both dark and light presets, so the cell
    // always has a visible band.
    if let Some(bg) = default_bg() {
        return Style::default().bg(user_message_bg(bg));
    }
    let theme = super::theme::current();
    Style::default().bg(theme.selected_bg)
}

pub(crate) fn composer_surface_style() -> Style {
    if let Some(bg) = default_bg() {
        return Style::default().bg(composer_surface_bg(bg));
    }
    let theme = super::theme::current();
    Style::default().bg(theme.selected_bg)
}

#[allow(dead_code)]
pub(crate) fn proposed_plan_style() -> Style {
    proposed_plan_style_for(default_bg())
}

pub(crate) fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => Style::default(),
    }
}

pub(crate) fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(proposed_plan_bg(bg)),
        None => Style::default(),
    }
}

fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.04)
    } else {
        // Keep the panel visibly lighter than the terminal surface so
        // user turns read as deliberate cards rather than faint bands.
        ((255, 255, 255), 0.30)
    };
    best_color(blend(top, terminal_bg, alpha))
}

fn composer_surface_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.03)
    } else {
        ((255, 255, 255), 0.18)
    };
    best_color(blend(top, terminal_bg, alpha))
}

fn proposed_plan_bg(terminal_bg: (u8, u8, u8)) -> Color {
    user_message_bg(terminal_bg)
}
