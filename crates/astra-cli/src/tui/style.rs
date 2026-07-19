use super::color::{blend, is_light};
use super::terminal_palette::{best_color, default_bg};
use ratatui::style::{Color, Style};

pub(crate) fn user_message_style() -> Style {
    // A user turn needs a quiet surface, not a gray card. Fall back to the
    // theme selection surface when the terminal background is unavailable so
    // the conversational boundary remains visible without an opaque block.
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

/// Background for the deferred-follow-up queue panel.
///
/// Deliberately tinted *differently* from the composer surface so the
/// queued-input band reads as a distinct region above the live input
/// box, not as more of the same surface. A touch darker than the
/// composer surface on dark backgrounds, a touch lighter on light ones.
pub(crate) fn queue_panel_style() -> Style {
    if let Some(bg) = default_bg() {
        return Style::default().bg(queue_panel_bg(bg));
    }
    let theme = super::theme::current();
    Style::default().bg(theme.selected_bg)
}

pub(crate) fn footer_surface_style() -> Style {
    if let Some(bg) = default_bg() {
        return Style::default().bg(footer_surface_bg(bg));
    }
    Style::default()
}

pub(crate) fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => Style::default().bg(super::theme::current().selected_bg),
    }
}

pub(crate) fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(proposed_plan_bg(bg)),
        None => Style::default(),
    }
}

fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    best_color(user_message_rgb(terminal_bg))
}

fn user_message_rgb(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.04)
    } else {
        // A restrained blue-slate lift distinguishes the user's turn without
        // turning a one-line message into a large disabled-looking gray card.
        ((84, 111, 145), 0.18)
    };
    blend(top, terminal_bg, alpha)
}

fn composer_surface_bg(terminal_bg: (u8, u8, u8)) -> Color {
    best_color(composer_surface_rgb(terminal_bg))
}

fn composer_surface_rgb(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.06)
    } else {
        ((84, 111, 145), 0.16)
    };
    blend(top, terminal_bg, alpha)
}

/// Distinct tint for the queue panel. Sits between the raw terminal
/// background and the composer surface in prominence: more present than
/// bare bg (so the band reads as a real region, not a gap) but less
/// lifted than the live composer (so the user's typing surface stays the
/// focal point). Previously 0.18 white was too close to a black terminal
/// bg — the panel vanished and the queued content looked unanchored.
fn queue_panel_bg(terminal_bg: (u8, u8, u8)) -> Color {
    best_color(queue_panel_rgb(terminal_bg))
}

fn queue_panel_rgb(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.10)
    } else {
        ((84, 111, 145), 0.10)
    };
    blend(top, terminal_bg, alpha)
}

fn footer_surface_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.015)
    } else {
        ((255, 255, 255), 0.04)
    };
    best_color(blend(top, terminal_bg, alpha))
}

fn proposed_plan_bg(terminal_bg: (u8, u8, u8)) -> Color {
    user_message_bg(terminal_bg)
}

#[cfg(test)]
mod tests {
    use super::{composer_surface_rgb, queue_panel_rgb, user_message_rgb};

    #[test]
    fn dark_conversation_surfaces_are_slate_not_opaque_gray_cards() {
        let terminal = (17, 22, 28);
        let (ur, ug, ub) = user_message_rgb(terminal);
        let (cr, cg, cb) = composer_surface_rgb(terminal);
        let (qr, qg, qb) = queue_panel_rgb(terminal);

        assert!(ub > ur && ub > ug, "user surface must keep a slate hue");
        assert!(ur < 50 && ug < 55 && ub < 65, "user surface is too bright");
        assert!(
            cr >= qr && cg >= qg && cb >= qb,
            "composer should lead queue"
        );
        assert!(
            cr < 50 && cg < 55 && cb < 65,
            "composer must not become gray"
        );
    }
}
