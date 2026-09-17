use super::color::perceptual_distance;
use ratatui::style::Color;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StdoutColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
    Unknown,
}

pub(crate) fn stdout_color_level() -> StdoutColorLevel {
    color_level(supports_color::Stream::Stdout)
}

pub(crate) fn color_level(stream: supports_color::Stream) -> StdoutColorLevel {
    match supports_color::on_cached(stream) {
        Some(level) if level.has_16m => StdoutColorLevel::TrueColor,
        Some(level) if level.has_256 => StdoutColorLevel::Ansi256,
        Some(_) => StdoutColorLevel::Ansi16,
        None => StdoutColorLevel::Unknown,
    }
}

pub(crate) fn best_color(target: (u8, u8, u8)) -> Color {
    best_color_for(stdout_color_level(), target)
}

/// Quantize a desired RGB colour for a declared terminal capability level.
///
/// This is kept separate from [`best_color`] so a theme can choose a complete
/// 256-colour palette deterministically instead of treating xterm-256color as
/// a 16-colour terminal merely because it lacks truecolour support.
pub(crate) fn best_color_for(color_level: StdoutColorLevel, target: (u8, u8, u8)) -> Color {
    match color_level {
        StdoutColorLevel::TrueColor => Color::Rgb(target.0, target.1, target.2),
        StdoutColorLevel::Ansi256 => {
            if let Some((i, _)) = xterm_fixed_colors().min_by(|(_, a), (_, b)| {
                perceptual_distance(*a, target)
                    .partial_cmp(&perceptual_distance(*b, target))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                Color::Indexed(i as u8)
            } else {
                Color::default()
            }
        }
        StdoutColorLevel::Ansi16 | StdoutColorLevel::Unknown => Color::default(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DefaultColors {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalColors {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

pub(crate) fn default_fg() -> Option<(u8, u8, u8)> {
    DEFAULT_COLORS
        .get_or_init(|| resolve_env_colors(TerminalColors::default()))
        .fg
}

pub(crate) fn default_bg() -> Option<(u8, u8, u8)> {
    DEFAULT_COLORS
        .get_or_init(|| resolve_env_colors(TerminalColors::default()))
        .bg
}

/// Called by interactive startup before the banner or theme is rendered.
/// Getters never perform terminal I/O, and late replies cannot change the theme.
pub(crate) fn initialize_default_colors(queried: TerminalColors) {
    let colors_set = DEFAULT_COLORS.set(resolve_env_colors(queried)).is_ok();
    let initialized = colors_set && !super::theme::is_initialized();
    if !initialized {
        tracing::warn!("terminal colors were initialized before the startup query completed");
    }
    debug_assert!(
        initialized,
        "terminal colors must be initialized before theme access"
    );
}

pub(crate) fn has_background_override() -> bool {
    std::env::var("ASTRA_TERMINAL_BG")
        .ok()
        .and_then(|value| parse_color_spec(value.trim()))
        .is_some()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OscColorSlot {
    Foreground,
    Background,
}

impl OscColorSlot {
    fn selector(self) -> &'static str {
        match self {
            Self::Foreground => "10",
            Self::Background => "11",
        }
    }
}

pub(crate) fn parse_osc_color_response(response: &str, slot: OscColorSlot) -> Option<(u8, u8, u8)> {
    let selector = slot.selector();
    let mut rest = response;
    while let Some(start) = rest.find("\x1b]") {
        rest = &rest[start + 2..];
        let Some((body, tail)) = split_osc_body(rest) else {
            break;
        };
        rest = tail;
        let Some((got_selector, spec)) = body.split_once(';') else {
            continue;
        };
        if got_selector == selector
            && let Some(rgb) = parse_color_spec(spec.trim())
        {
            return Some(rgb);
        }
    }
    None
}

fn split_osc_body(s: &str) -> Option<(&str, &str)> {
    let bel = s.find('\x07');
    let st = s.find("\x1b\\");
    match (bel, st) {
        (Some(b), Some(t)) if b < t => Some((&s[..b], &s[b + 1..])),
        (Some(_), Some(t)) => Some((&s[..t], &s[t + 2..])),
        (Some(b), None) => Some((&s[..b], &s[b + 1..])),
        (None, Some(t)) => Some((&s[..t], &s[t + 2..])),
        (None, None) => None,
    }
}

fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = spec.strip_prefix('#') {
        return parse_hex_rgb(hex);
    }
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r = parse_x_color_component(parts.next()?)?;
        let g = parse_x_color_component(parts.next()?)?;
        let b = parse_x_color_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    if let Some(rest) = spec.strip_prefix("rgba:") {
        let mut parts = rest.split('/');
        let r = parse_x_color_component(parts.next()?)?;
        let g = parse_x_color_component(parts.next()?)?;
        let b = parse_x_color_component(parts.next()?)?;
        let _alpha = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        return Some((r, g, b));
    }
    let mut parts = spec.split(',');
    let r = parts.next()?.trim().parse::<u8>().ok()?;
    let g = parts.next()?.trim().parse::<u8>().ok()?;
    let b = parts.next()?.trim().parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    if !hex.is_ascii() {
        return None;
    }
    match hex.len() {
        6 => Some((
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        )),
        12 => Some((
            parse_x_color_component(&hex[0..4])?,
            parse_x_color_component(&hex[4..8])?,
            parse_x_color_component(&hex[8..12])?,
        )),
        _ => None,
    }
}

fn parse_x_color_component(part: &str) -> Option<u8> {
    if part.is_empty() || part.len() > 4 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u16::from_str_radix(part, 16).ok()?;
    let max = (1u32 << (part.len() * 4)) - 1;
    Some(((value as u32 * 255 + max / 2) / max) as u8)
}

fn resolve_env_colors(queried: TerminalColors) -> TerminalColors {
    let fg = std::env::var("ASTRA_TERMINAL_FG").ok();
    let bg = std::env::var("ASTRA_TERMINAL_BG").ok();
    let hint = std::env::var("COLORFGBG").ok();
    resolve_colors(fg.as_deref(), bg.as_deref(), queried, hint.as_deref())
}

fn resolve_colors(
    fg: Option<&str>,
    bg: Option<&str>,
    queried: TerminalColors,
    hint: Option<&str>,
) -> TerminalColors {
    let hint = hint.and_then(|value| parse_colorfgbg(value.trim()));
    TerminalColors {
        fg: fg
            .and_then(|value| parse_color_spec(value.trim()))
            .or(queried.fg)
            .or(hint.map(|colors| colors.fg)),
        bg: bg
            .and_then(|value| parse_color_spec(value.trim()))
            .or(queried.bg)
            .or(hint.map(|colors| colors.bg)),
    }
}

fn parse_colorfgbg(value: &str) -> Option<DefaultColors> {
    let (prefix, bg) = value.rsplit_once(';')?;
    // rxvt may include an intermediate background field.
    let fg = prefix.split(';').next()?;
    Some(DefaultColors {
        fg: ansi_index_rgb(fg.trim().parse::<u8>().ok()?),
        bg: ansi_index_rgb(bg.trim().parse::<u8>().ok()?),
    })
}

fn ansi_index_rgb(index: u8) -> (u8, u8, u8) {
    XTERM_COLORS[index as usize]
}

static DEFAULT_COLORS: OnceLock<TerminalColors> = OnceLock::new();

fn xterm_fixed_colors() -> impl Iterator<Item = (usize, (u8, u8, u8))> {
    XTERM_COLORS.into_iter().enumerate().skip(16)
}

#[rustfmt::skip]
pub(crate) const XTERM_COLORS: [(u8, u8, u8); 256] = [
    (0,0,0),(128,0,0),(0,128,0),(128,128,0),(0,0,128),(128,0,128),(0,128,128),(192,192,192),
    (128,128,128),(255,0,0),(0,255,0),(255,255,0),(0,0,255),(255,0,255),(0,255,255),(255,255,255),
    (0,0,0),(0,0,95),(0,0,135),(0,0,175),(0,0,215),(0,0,255),(0,95,0),(0,95,95),
    (0,95,135),(0,95,175),(0,95,215),(0,95,255),(0,135,0),(0,135,95),(0,135,135),(0,135,175),
    (0,135,215),(0,135,255),(0,175,0),(0,175,95),(0,175,135),(0,175,175),(0,175,215),(0,175,255),
    (0,215,0),(0,215,95),(0,215,135),(0,215,175),(0,215,215),(0,215,255),(0,255,0),(0,255,95),
    (0,255,135),(0,255,175),(0,255,215),(0,255,255),(95,0,0),(95,0,95),(95,0,135),(95,0,175),
    (95,0,215),(95,0,255),(95,95,0),(95,95,95),(95,95,135),(95,95,175),(95,95,215),(95,95,255),
    (95,135,0),(95,135,95),(95,135,135),(95,135,175),(95,135,215),(95,135,255),(95,175,0),(95,175,95),
    (95,175,135),(95,175,175),(95,175,215),(95,175,255),(95,215,0),(95,215,95),(95,215,135),(95,215,175),
    (95,215,215),(95,215,255),(95,255,0),(95,255,95),(95,255,135),(95,255,175),(95,255,215),(95,255,255),
    (135,0,0),(135,0,95),(135,0,135),(135,0,175),(135,0,215),(135,0,255),(135,95,0),(135,95,95),
    (135,95,135),(135,95,175),(135,95,215),(135,95,255),(135,135,0),(135,135,95),(135,135,135),(135,135,175),
    (135,135,215),(135,135,255),(135,175,0),(135,175,95),(135,175,135),(135,175,175),(135,175,215),(135,175,255),
    (135,215,0),(135,215,95),(135,215,135),(135,215,175),(135,215,215),(135,215,255),(135,255,0),(135,255,95),
    (135,255,135),(135,255,175),(135,255,215),(135,255,255),(175,0,0),(175,0,95),(175,0,135),(175,0,175),
    (175,0,215),(175,0,255),(175,95,0),(175,95,95),(175,95,135),(175,95,175),(175,95,215),(175,95,255),
    (175,135,0),(175,135,95),(175,135,135),(175,135,175),(175,135,215),(175,135,255),(175,175,0),(175,175,95),
    (175,175,135),(175,175,175),(175,175,215),(175,175,255),(175,215,0),(175,215,95),(175,215,135),(175,215,175),
    (175,215,215),(175,215,255),(175,255,0),(175,255,95),(175,255,135),(175,255,175),(175,255,215),(175,255,255),
    (215,0,0),(215,0,95),(215,0,135),(215,0,175),(215,0,215),(215,0,255),(215,95,0),(215,95,95),
    (215,95,135),(215,95,175),(215,95,215),(215,95,255),(215,135,0),(215,135,95),(215,135,135),(215,135,175),
    (215,135,215),(215,135,255),(215,175,0),(215,175,95),(215,175,135),(215,175,175),(215,175,215),(215,175,255),
    (215,215,0),(215,215,95),(215,215,135),(215,215,175),(215,215,215),(215,215,255),(215,255,0),(215,255,95),
    (215,255,135),(215,255,175),(215,255,215),(215,255,255),(255,0,0),(255,0,95),(255,0,135),(255,0,175),
    (255,0,215),(255,0,255),(255,95,0),(255,95,95),(255,95,135),(255,95,175),(255,95,215),(255,95,255),
    (255,135,0),(255,135,95),(255,135,135),(255,135,175),(255,135,215),(255,135,255),(255,175,0),(255,175,95),
    (255,175,135),(255,175,175),(255,175,215),(255,175,255),(255,215,0),(255,215,95),(255,215,135),(255,215,175),
    (255,215,215),(255,215,255),(255,255,0),(255,255,95),(255,255,135),(255,255,175),(255,255,215),(255,255,255),
    (8,8,8),(18,18,18),(28,28,28),(38,38,38),(48,48,48),(58,58,58),(68,68,68),(78,78,78),
    (88,88,88),(98,98,98),(108,108,108),(118,118,118),(128,128,128),(138,138,138),(148,148,148),(158,158,158),
    (168,168,168),(178,178,178),(188,188,188),(198,198,198),(208,208,208),(218,218,218),(228,228,228),(238,238,238),
];

#[cfg(test)]
mod tests {
    use super::{
        DefaultColors, OscColorSlot, StdoutColorLevel, best_color_for, parse_color_spec,
        parse_colorfgbg, parse_osc_color_response,
    };
    use ratatui::style::Color;

    #[test]
    fn ansi256_quantization_keeps_a_real_indexed_colour() {
        assert!(matches!(
            best_color_for(StdoutColorLevel::Ansi256, (19, 49, 40)),
            Color::Indexed(_)
        ));
        assert_eq!(
            best_color_for(StdoutColorLevel::Ansi16, (19, 49, 40)),
            Color::Reset
        );
    }

    #[test]
    fn parses_osc11_rgb_response_with_st_terminator() {
        let response = "\x1b]11;rgb:ffff/eeee/dddd\x1b\\";
        assert_eq!(
            parse_osc_color_response(response, OscColorSlot::Background),
            Some((255, 238, 221))
        );
    }

    #[test]
    fn parses_osc10_hash_response_with_bel_terminator() {
        let response = "\x1b]10;#102030\x07";
        assert_eq!(
            parse_osc_color_response(response, OscColorSlot::Foreground),
            Some((16, 32, 48))
        );
    }

    #[test]
    fn ignores_wrong_osc_selector_and_malformed_color() {
        assert_eq!(
            parse_osc_color_response("\x1b]10;rgb:ffff/eeee/dddd\x1b\\", OscColorSlot::Background),
            None
        );
        assert_eq!(
            parse_osc_color_response("\x1b]11;rgb:ffff/nope/dddd\x1b\\", OscColorSlot::Background),
            None
        );
    }

    #[test]
    fn parses_env_color_specs() {
        assert_eq!(parse_color_spec("#abcdef"), Some((171, 205, 239)));
        assert_eq!(parse_color_spec("10,20,30"), Some((10, 20, 30)));
        assert_eq!(parse_color_spec("rgb:0000/8000/ffff"), Some((0, 128, 255)));
        assert_eq!(parse_color_spec("#你abc"), None);
        assert_eq!(parse_color_spec("#a你bc"), None);
    }

    #[test]
    fn manual_colors_override_queries_and_queries_override_environment_hints() {
        let queried = super::TerminalColors {
            fg: Some((1, 2, 3)),
            bg: Some((255, 255, 255)),
        };
        let resolved = super::resolve_colors(Some("#102030"), None, queried, Some("15;0"));
        assert_eq!(resolved.fg, Some((16, 32, 48)));
        assert_eq!(resolved.bg, Some((255, 255, 255)));
        let resolved = super::resolve_colors(None, Some("#000000"), queried, Some("0;15"));
        assert_eq!(resolved.fg, Some((1, 2, 3)));
        assert_eq!(resolved.bg, Some((0, 0, 0)));
    }

    #[test]
    fn a_background_response_or_override_is_sufficient_without_a_foreground() {
        let queried = super::TerminalColors {
            fg: None,
            bg: Some((255, 255, 255)),
        };
        assert_eq!(super::resolve_colors(None, None, queried, None), queried);
        assert_eq!(
            super::resolve_colors(
                None,
                Some("#ffffff"),
                super::TerminalColors::default(),
                None
            ),
            queried,
        );
    }

    #[test]
    fn unavailable_or_malformed_response_falls_back_per_channel() {
        let queried = super::TerminalColors::default();
        assert_eq!(super::resolve_colors(None, None, queried, None), queried);
        let colors = super::resolve_colors(Some("bad"), Some("bad"), queried, Some("0;15"));
        assert_eq!(colors.fg, Some((0, 0, 0)));
        assert_eq!(colors.bg, Some((255, 255, 255)));
        assert_eq!(parse_colorfgbg("0;default;15"), parse_colorfgbg("0;15"));
    }

    #[test]
    fn manual_colors_override_environment_hints_per_channel() {
        let queried = super::TerminalColors::default();
        let resolved = super::resolve_colors(Some("#102030"), None, queried, Some("15;0"));
        assert_eq!(resolved.fg, Some((16, 32, 48)));
        assert_eq!(resolved.bg, Some((0, 0, 0)));
        let resolved = super::resolve_colors(None, Some("#000000"), queried, Some("0;15"));
        assert_eq!(resolved.fg, Some((0, 0, 0)));
        assert_eq!(resolved.bg, Some((0, 0, 0)));
    }

    #[test]
    fn manual_colors_work_independently_without_environment_hints() {
        let queried = super::TerminalColors::default();
        assert_eq!(
            super::resolve_colors(None, Some("#ffffff"), queried, None),
            super::TerminalColors {
                fg: None,
                bg: Some((255, 255, 255))
            },
        );
        assert_eq!(
            super::resolve_colors(Some("#102030"), None, queried, None),
            super::TerminalColors {
                fg: Some((16, 32, 48)),
                bg: None
            },
        );
    }

    #[test]
    fn malformed_overrides_fall_back_to_environment_hints() {
        let queried = super::TerminalColors::default();
        assert_eq!(super::resolve_colors(None, None, queried, None), queried);
        let colors = super::resolve_colors(Some("bad"), Some("bad"), queried, Some("0;15"));
        assert_eq!(colors.fg, Some((0, 0, 0)));
        assert_eq!(colors.bg, Some((255, 255, 255)));
        assert_eq!(parse_colorfgbg("0;default;15"), parse_colorfgbg("0;15"));
        let colors = parse_colorfgbg("232;255").unwrap();
        assert_eq!(colors.fg, (8, 8, 8));
        assert_eq!(colors.bg, (238, 238, 238));
    }

    #[test]
    fn parses_colorfgbg_indexes() {
        assert_eq!(
            parse_colorfgbg("15;0"),
            Some(DefaultColors {
                fg: (255, 255, 255),
                bg: (0, 0, 0),
            })
        );
        assert_eq!(
            parse_colorfgbg("7;10"),
            Some(DefaultColors {
                fg: (192, 192, 192),
                bg: (0, 255, 0),
            })
        );
    }
}
