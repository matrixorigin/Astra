use super::color::{blend, is_light};
use super::terminal_palette::{best_color, default_bg};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Shared inset for conversation, composer and execution-report chrome.
pub(crate) fn conversation_margin(width: u16) -> u16 {
    if width >= 60 { 2 } else { 0 }
}

/// Use one local clock format for recorded conversation headings.
pub(crate) fn append_local_time(line: &mut Line<'static>, timestamp: Option<&str>, width: u16) {
    let Some(timestamp) = timestamp else {
        return;
    };
    if line.width() + "  HH:MM:SS".len() > usize::from(width) {
        return;
    }
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        line.spans.push(Span::styled(
            time.with_timezone(&chrono::Local)
                .format("  %H:%M:%S")
                .to_string(),
            Style::default().fg(super::theme::current().dim),
        ));
    }
}

/// Presentation-only status vocabulary. Owners map their typed runtime state
/// to this enum; rendered text is never inspected to recover a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Running,
    Waiting,
    Success,
    Warning,
    Failure,
    Cancelled,
    Interrupted,
    Unknown,
}

pub(crate) fn status_style(kind: StatusKind) -> Style {
    status_style_for(kind, super::theme::current())
}

fn status_style_for(kind: StatusKind, theme: &super::theme::Theme) -> Style {
    let color = match kind {
        StatusKind::Running => theme.accent,
        StatusKind::Waiting | StatusKind::Unknown => theme.dim,
        StatusKind::Success => theme.success,
        StatusKind::Warning | StatusKind::Cancelled | StatusKind::Interrupted => theme.warn,
        StatusKind::Failure => theme.error,
    };
    let style = Style::default().fg(color);
    if matches!(kind, StatusKind::Running | StatusKind::Failure) {
        style.bold()
    } else {
        style
    }
}

pub(crate) fn status_glyph(kind: StatusKind) -> &'static str {
    status_glyph_for(kind, super::glyphs::current())
}

fn status_glyph_for(kind: StatusKind, glyphs: &super::glyphs::Glyphs) -> &'static str {
    match kind {
        StatusKind::Running => glyphs.agent_running,
        StatusKind::Waiting => glyphs.agent_waiting,
        StatusKind::Success => glyphs.agent_completed,
        StatusKind::Warning => glyphs.agent_stale,
        StatusKind::Failure => glyphs.agent_failed,
        StatusKind::Cancelled => glyphs.agent_cancelled,
        StatusKind::Interrupted => glyphs.agent_interrupted,
        StatusKind::Unknown => glyphs.agent_unconfirmed,
    }
}

pub(crate) fn status_marker(kind: StatusKind) -> Span<'static> {
    Span::styled(format!("{} ", status_glyph(kind)), status_style(kind))
}

pub(crate) fn user_message_style() -> Style {
    user_message_style_for(default_bg())
}

pub(crate) fn composer_surface_style() -> Style {
    surface_style(super::theme::current(), default_bg(), composer_surface_rgb)
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

#[cfg(test)]
mod status_tests {
    use super::{StatusKind, status_glyph_for, status_marker, status_style_for};
    use crate::tui::{
        glyphs::{GlyphProfile, for_profile},
        theme::Theme,
    };
    use std::collections::BTreeSet;

    const STATES: [StatusKind; 8] = [
        StatusKind::Running,
        StatusKind::Waiting,
        StatusKind::Success,
        StatusKind::Warning,
        StatusKind::Failure,
        StatusKind::Cancelled,
        StatusKind::Interrupted,
        StatusKind::Unknown,
    ];

    #[test]
    fn semantic_status_styles_follow_every_theme() {
        for theme in [
            Theme::dark(),
            Theme::light(),
            Theme::dark_ansi(),
            Theme::plain(),
        ] {
            assert_eq!(
                status_style_for(StatusKind::Running, &theme).fg,
                Some(theme.accent)
            );
            assert_eq!(
                status_style_for(StatusKind::Success, &theme).fg,
                Some(theme.success)
            );
            assert_eq!(
                status_style_for(StatusKind::Warning, &theme).fg,
                Some(theme.warn)
            );
            assert_eq!(
                status_style_for(StatusKind::Failure, &theme).fg,
                Some(theme.error)
            );
            assert_eq!(
                status_style_for(StatusKind::Waiting, &theme).fg,
                Some(theme.dim)
            );
        }
    }

    #[test]
    fn unicode_and_ascii_status_glyphs_stay_distinct() {
        for profile in [GlyphProfile::Unicode, GlyphProfile::Ascii] {
            let glyphs = for_profile(profile);
            let distinct: BTreeSet<_> = STATES
                .iter()
                .map(|state| status_glyph_for(*state, glyphs))
                .collect();
            assert_eq!(distinct.len(), STATES.len());
        }
    }

    #[test]
    fn marker_owns_exactly_one_trailing_space() {
        let marker = status_marker(StatusKind::Success);
        assert!(marker.content.ends_with(' '));
        assert!(!marker.content.ends_with("  "));
    }
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
