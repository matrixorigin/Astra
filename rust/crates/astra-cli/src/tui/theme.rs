//! TUI color theme — one source of truth for every semantic colour.
//!
//! Background: this codebase had 40+ hardcoded `Color::Black` /
//! `Color::White` / `Color::Cyan` calls scattered across render paths,
//! which broke on light terminals (black-on-cyan reverses to invisible
//! on a light background). The [`Theme`] struct names every semantic
//! slot (accent, selected_bg, error, …) and two presets — [`Theme::dark`]
//! and [`Theme::light`] — realise them for either environment.
//!
//! Callers should go through [`current()`] rather than instantiating
//! directly. `current()` caches the auto-selected theme once per
//! process based on [`terminal_palette::default_bg`], then never
//! changes — terminal colours rarely flip mid-session.

#![allow(dead_code)]

use std::sync::OnceLock;

use ratatui::style::Color;

use super::color::blend;

/// Named semantic slots. Every TUI render path should read through
/// `Theme::current()` rather than embedding `Color::X` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    /// Whether this theme targets a light-backgrounded terminal.
    pub is_light: bool,
    /// Plain text on default background.
    pub fg: Color,
    /// Dim / secondary text.
    pub dim: Color,
    /// Brand / focus / headings. Typically cyan on dark, deep blue on light.
    pub accent: Color,
    /// Subtle "selected" background blend used to highlight rows.
    pub selected_bg: Color,
    /// Foreground on top of `selected_bg`. Guaranteed readable.
    pub selected_fg: Color,
    /// Gutter indicator colour (same as accent in current presets).
    pub gutter: Color,
    pub success: Color,
    pub warn: Color,
    pub error: Color,
    /// Blockquote / secondary-emphasis hue.
    pub quote: Color,
    /// Link URL colour.
    pub link: Color,
}

impl Theme {
    /// Preset for dark-backgrounded terminals (the previous default).
    pub fn dark() -> Self {
        Self {
            is_light: false,
            fg: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Magenta,
            // Clearly visible grey band for user input (matches Claude Code).
            selected_bg: Color::Rgb(55, 55, 60),
            selected_fg: Color::Rgb(232, 220, 245),
            // Soft pink for the assistant-reply gutter (`┃ `) — reads
            // clearly on any dark terminal background while feeling
            // warmer than the previous cyan accent and providing a
            // visual break between the composer accent (cyan) and
            // the model's output.
            gutter: Color::Rgb(246, 168, 195),
            success: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            quote: Color::Green,
            link: Color::Cyan,
        }
    }

    /// Preset for light-backgrounded terminals.
    pub fn light() -> Self {
        Self {
            is_light: true,
            fg: Color::Reset,
            dim: Color::Gray,
            accent: Color::Rgb(148, 40, 148), // deep magenta
            selected_bg: Color::Rgb(245, 232, 250),
            selected_fg: Color::Rgb(24, 17, 35),
            gutter: Color::Rgb(180, 60, 120),
            success: Color::Rgb(22, 115, 46),
            warn: Color::Rgb(135, 89, 0),
            error: Color::Rgb(170, 34, 34),
            quote: Color::Rgb(80, 110, 80),
            link: Color::Rgb(148, 40, 148),
        }
    }

    /// Select a preset automatically based on terminal background.
    /// Falls back to `dark` when no signal is available.
    pub fn auto() -> Self {
        if is_light_background() {
            Self::light()
        } else {
            Self::dark()
        }
    }

    /// Produce a dimmer variant of the accent for low-emphasis uses.
    pub fn accent_dim(&self) -> Color {
        let acc = color_to_rgb(self.accent);
        let bg = if self.is_light {
            (240, 240, 240)
        } else {
            (17, 17, 17)
        };
        let (r, g, b) = blend(bg, acc, 0.6);
        Color::Rgb(r, g, b)
    }
}

fn is_light_background() -> bool {
    // 1. Direct query from terminal (disabled on crossterm 0.28 for now,
    //    but the helper returns Some if ever re-enabled).
    if let Some((r, g, b)) = super::terminal_palette::default_bg() {
        return perceived_lightness(r, g, b) > 0.5;
    }
    // 2. COLORFGBG env var (e.g. "15;0" meaning fg=15 bg=0).
    if let Ok(v) = std::env::var("COLORFGBG")
        && let Some((_, bg_str)) = v.split_once(';')
        && let Ok(bg_idx) = bg_str.trim().parse::<u8>()
    {
        // Low indices 0..=7 are dark; 8..=15 mixed; but in practice a
        // COLORFGBG with bg>=10 is *very* likely a light terminal.
        return bg_idx >= 10;
    }
    false
}

fn perceived_lightness(r: u8, g: u8, b: u8) -> f32 {
    // sRGB → perceived luma (quick approximation).
    (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
}

fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (128, 128, 128),
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Process-wide theme, chosen once at first access. Tests that need a
/// specific theme should call [`set_for_tests`] *before* any `current()`.
pub(crate) fn current() -> &'static Theme {
    THEME.get_or_init(Theme::auto)
}

#[cfg(test)]
pub(crate) fn set_for_tests(theme: Theme) {
    let _ = THEME.set(theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_are_distinct() {
        assert!(!Theme::dark().is_light);
        assert!(Theme::light().is_light);
        assert_ne!(Theme::dark().accent, Theme::light().accent);
    }

    #[test]
    fn perceived_lightness_basic_bounds() {
        assert!(perceived_lightness(0, 0, 0) < 0.1);
        assert!(perceived_lightness(255, 255, 255) > 0.9);
        // Mid-grey
        let mid = perceived_lightness(128, 128, 128);
        assert!(mid > 0.4 && mid < 0.6);
    }

    #[test]
    fn accent_dim_is_different_from_accent() {
        let d = Theme::dark();
        assert_ne!(d.accent_dim(), d.accent);
    }

    #[test]
    fn light_theme_readable_selection_contrast() {
        // Selected_fg should be dark enough on a light selected_bg.
        let l = Theme::light();
        let (br, bg, bb) = color_to_rgb(l.selected_bg);
        let (fr, fg, fb) = color_to_rgb(l.selected_fg);
        let bg_l = perceived_lightness(br, bg, bb);
        let fg_l = perceived_lightness(fr, fg, fb);
        assert!(
            (bg_l - fg_l).abs() > 0.5,
            "light theme needs strong selected bg/fg contrast; got bg={bg_l:.2} fg={fg_l:.2}"
        );
    }

    #[test]
    fn dark_theme_readable_selection_contrast() {
        let d = Theme::dark();
        let (br, bg, bb) = color_to_rgb(d.selected_bg);
        let (fr, fg, fb) = color_to_rgb(d.selected_fg);
        let bg_l = perceived_lightness(br, bg, bb);
        let fg_l = perceived_lightness(fr, fg, fb);
        assert!((bg_l - fg_l).abs() > 0.5);
    }

    #[test]
    fn auto_without_signal_is_dark() {
        // With no COLORFGBG and no terminal query, auto must default to dark.
        let original = std::env::var("COLORFGBG").ok();
        // SAFETY: tests in this file run single-threaded within the process
        // via cargo test's default, but env writes can race other tests.
        // The tradeoff here is limited to the audit case below.
        unsafe { std::env::remove_var("COLORFGBG") };
        let t = Theme::auto();
        assert!(!t.is_light);
        if let Some(v) = original {
            unsafe { std::env::set_var("COLORFGBG", v) };
        }
    }

    #[test]
    fn current_is_stable() {
        // Calling current twice returns the same cached theme.
        let a = current();
        let b = current();
        assert_eq!(a as *const _, b as *const _);
    }
}
