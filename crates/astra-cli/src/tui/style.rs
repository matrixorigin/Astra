use super::color::{blend, is_light};
use super::terminal_palette::{best_color, default_bg};
use ratatui::style::{Color, Style};

pub(crate) fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub(crate) fn composer_surface_style() -> Style {
    surface_style(super::theme::current(), default_bg(), composer_surface_rgb)
}

/// Background and foreground for the deferred-follow-up queue panel.
pub(crate) fn queue_panel_style() -> Style {
    surface_style(super::theme::current(), default_bg(), queue_panel_rgb)
}

pub(crate) fn footer_surface_style() -> Style {
    // The footer uses terminal defaults rather than an opaque panel.
    Style::default()
}

pub(crate) fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    surface_style(super::theme::current(), terminal_bg, user_message_rgb)
}

pub(crate) fn proposed_plan_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    user_message_style_for(terminal_bg)
}

type SurfaceTint = fn((u8, u8, u8)) -> (u8, u8, u8);

fn surface_style(
    theme: &super::theme::Theme,
    terminal_bg: Option<(u8, u8, u8)>,
    tint: SurfaceTint,
) -> Style {
    // Plain/unknown-background surfaces must stay transparent even when hints
    // are present. Explicit theme selection wins over conflicting hints.
    if theme.selected_bg == Color::Reset {
        return Style::default();
    }
    let background = match terminal_bg {
        Some(bg)
            if is_light(bg) == theme.is_light
                && matches!(theme.selected_bg, Color::Rgb(..) | Color::Indexed(_)) =>
        {
            let tinted = best_color(tint(bg));
            if tinted == Color::Reset {
                theme.selected_bg
            } else {
                tinted
            }
        }
        _ => theme.selected_bg,
    };
    Style::default().bg(background).fg(theme.selected_fg)
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
fn queue_panel_rgb(terminal_bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let (top, alpha) = if is_light(terminal_bg) {
        ((0, 0, 0), 0.10)
    } else {
        ((84, 111, 145), 0.10)
    };
    blend(top, terminal_bg, alpha)
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

#[cfg(test)]
mod contrast_tests {
    use super::*;
    use crate::tui::theme::Theme;

    #[test]
    fn conversation_surfaces_pair_foreground_and_background() {
        for theme in [
            Theme::light(),
            Theme::dark(),
            Theme::light_ansi(),
            Theme::dark_ansi(),
            Theme::light_256(),
            Theme::dark_256(),
        ] {
            for background in [None, Some((255, 255, 255)), Some((17, 22, 28))] {
                for tint in [user_message_rgb, composer_surface_rgb, queue_panel_rgb] {
                    let style = surface_style(&theme, background, tint);
                    assert_eq!(style.fg, Some(theme.selected_fg));
                    assert_ne!(style.bg, Some(Color::Reset));
                    if background.is_none()
                        || background.is_some_and(|bg| is_light(bg) != theme.is_light)
                    {
                        assert_eq!(style.bg, Some(theme.selected_bg));
                    }
                }
            }
        }
    }

    #[test]
    fn plain_surfaces_ignore_background_hints() {
        for theme in [Theme::plain(), Theme::terminal_default()] {
            for background in [None, Some((255, 255, 255)), Some((17, 22, 28))] {
                for tint in [user_message_rgb, composer_surface_rgb, queue_panel_rgb] {
                    assert_eq!(surface_style(&theme, background, tint), Style::default());
                }
            }
        }
    }
}
