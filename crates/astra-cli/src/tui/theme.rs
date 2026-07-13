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
//! directly. `current()` caches the selected theme once per process. It uses
//! terminal capability and background hints, while `ASTRA_TUI_THEME` gives a
//! user an explicit escape hatch for a multiplexer or remote terminal whose
//! capability environment is incomplete.

#![allow(dead_code)]

use std::sync::OnceLock;

use ratatui::style::{Color, Style};

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
    /// Gutter indicator colour for live (streaming) cells.
    pub gutter: Color,
    /// Muted gutter colour for settled (frozen) cells.
    pub gutter_frozen: Color,
    pub success: Color,
    pub warn: Color,
    pub error: Color,
    /// Blockquote / secondary-emphasis hue.
    pub quote: Color,
    /// Link URL colour.
    pub link: Color,
    /// Dimmed directory-portion of a file path (e.g. `src/tui/`).
    pub path_dim: Color,
    /// Bright filename-portion of a file path (e.g. `tool.rs`).
    pub path_file: Color,
    /// Shell command text (e.g. the `git diff` in `$ git diff --stat`).
    pub command: Color,
    // ── Markdown semantic slots ──────────────────────────────────────────
    /// Heading text (h1/h2/h3).
    pub md_heading: Color,
    /// Inline code (e.g. `code`).
    pub md_code: Color,
    /// Hyperlink text.
    pub md_link: Color,
    /// Blockquote text (e.g. `> quote`).
    pub md_blockquote: Color,
    /// Ordered/unordered list markers.
    pub md_list_marker: Color,
    // ── Diff semantic slots ──────────────────────────────────────────────
    /// Added line foreground (colorblind-friendly: blue-green tint).
    pub diff_add_fg: Color,
    /// Added line background.
    pub diff_add_bg: Color,
    /// Deleted line foreground (colorblind-friendly: orange-red tint).
    pub diff_del_fg: Color,
    /// Deleted line background.
    pub diff_del_bg: Color,
    /// Hunk header (@@ ... @@).
    pub diff_hunk: Color,
    /// Context/unchanged lines.
    pub diff_context: Color,
    // ── Status indicator semantic slots ──────────────────────────────────
    /// Stall warning threshold color (5s).
    pub stall_warn: Color,
    /// Stall error threshold color (10s).
    pub stall_error: Color,
}

/// Rendering colour capability selected for the whole TUI lifetime.
///
/// This is deliberately a small presentation choice, not a second theme
/// framework: all renderers continue to consume the same semantic slots from
/// [`Theme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeProfile {
    Auto,
    Dark,
    Light,
    DarkAnsi,
    LightAnsi,
    Plain,
}

impl ThemeProfile {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "dark-ansi" | "ansi-dark" => Some(Self::DarkAnsi),
            "light-ansi" | "ansi-light" => Some(Self::LightAnsi),
            "plain" | "none" | "no-color" => Some(Self::Plain),
            _ => None,
        }
    }
}

impl Theme {
    /// Preset for dark-backgrounded terminals (the previous default).
    pub fn dark() -> Self {
        Self {
            is_light: false,
            fg: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Rgb(108, 169, 255),
            // A low-saturation slate surface keeps the composer and selected
            // rows legible without turning the TUI into a stack of cards.
            selected_bg: Color::Rgb(31, 42, 55),
            selected_fg: Color::Rgb(231, 238, 248),
            // Active work uses a vivid emerald gutter: it is a compact,
            // high-salience execution marker rather than another panel.
            // Settled output switches to a cool slate, not a dimmer green:
            // colour must communicate terminal state at a glance, even in a
            // peripheral transcript scan.
            gutter: Color::Rgb(43, 220, 142),
            gutter_frozen: Color::Rgb(91, 115, 136),
            success: Color::Rgb(54, 226, 157),
            warn: Color::Rgb(244, 192, 102),
            // Bright red is reserved for confirmed failure or dangerous
            // permission state. Ordinary activity never receives this role.
            error: Color::Rgb(255, 103, 112),
            quote: Color::Rgb(121, 171, 159),
            link: Color::Rgb(122, 181, 222),
            path_dim: Color::DarkGray,
            path_file: Color::White,
            command: Color::Rgb(88, 181, 255),
            // Markdown stays in the same restrained blue/slate language as
            // the rest of the workbench rather than introducing a second
            // purple/pink identity.
            md_heading: Color::Rgb(184, 209, 244),
            md_code: Color::Rgb(190, 210, 231),
            md_link: Color::Rgb(108, 169, 255),
            md_blockquote: Color::Rgb(117, 184, 161),
            md_list_marker: Color::Rgb(138, 184, 233),
            // The row surface groups a complete edit line; it must stay much
            // quieter than its foreground or a diff turns into a wall of
            // fluorescent rectangles.
            diff_add_fg: Color::Rgb(132, 231, 189),
            diff_add_bg: Color::Rgb(26, 43, 37),
            diff_del_fg: Color::Rgb(255, 163, 166),
            diff_del_bg: Color::Rgb(45, 34, 38),
            diff_hunk: Color::Rgb(108, 169, 255),
            diff_context: Color::DarkGray,
            // Status indicator: stall thresholds use warn/error colors.
            stall_warn: Color::Rgb(244, 192, 102),
            stall_error: Color::Rgb(255, 103, 112),
        }
    }

    /// Preset for light-backgrounded terminals.
    pub fn light() -> Self {
        Self {
            is_light: true,
            fg: Color::Reset,
            dim: Color::Gray,
            accent: Color::Rgb(39, 98, 149),
            selected_bg: Color::Rgb(226, 235, 243),
            selected_fg: Color::Rgb(23, 34, 45),
            gutter: Color::Rgb(48, 117, 153),
            gutter_frozen: Color::Rgb(89, 119, 139),
            success: Color::Rgb(33, 120, 101),
            warn: Color::Rgb(139, 100, 38),
            error: Color::Rgb(163, 66, 67),
            quote: Color::Rgb(51, 119, 104),
            link: Color::Rgb(39, 98, 149),
            path_dim: Color::Gray,
            path_file: Color::Black,
            command: Color::Rgb(25, 112, 147),
            md_heading: Color::Rgb(39, 98, 149),
            md_code: Color::Rgb(54, 109, 153),
            md_link: Color::Rgb(39, 98, 149),
            md_blockquote: Color::Rgb(51, 119, 104),
            md_list_marker: Color::Rgb(47, 106, 151),
            // Diff: GitHub-style pastels (colorblind-friendly).
            diff_add_fg: Color::Rgb(31, 35, 40),
            diff_add_bg: Color::Rgb(218, 251, 225),
            diff_del_fg: Color::Rgb(31, 35, 40),
            diff_del_bg: Color::Rgb(255, 235, 233),
            diff_hunk: Color::Rgb(39, 98, 149),
            diff_context: Color::Gray,
            // Status indicator: stall thresholds use warn/error colors.
            stall_warn: Color::Rgb(139, 100, 38),
            stall_error: Color::Rgb(163, 66, 67),
        }
    }

    /// xterm-256color palette. It preserves the same restrained visual
    /// hierarchy as the truecolour theme while quantizing every RGB token to
    /// an indexed colour the terminal can actually display.
    pub fn dark_256() -> Self {
        let mut theme = Self::quantize_256(Self::dark());
        // The xterm cube has no genuinely low-saturation dark red/green
        // surfaces: indices 22/52 turn a wide diff into solid green/red bars.
        // Keep the *row* grouping on a quiet graphite surface and carry
        // direction with the foreground plus the explicit +/- marker. This is
        // an intentional capability fallback, not a second visual language.
        theme.diff_add_fg = Color::Indexed(115);
        theme.diff_add_bg = Color::Indexed(234);
        theme.diff_del_fg = Color::Indexed(217);
        theme.diff_del_bg = Color::Indexed(234);
        theme
    }

    /// Indexed equivalent of [`Self::light`].
    pub fn light_256() -> Self {
        Self::quantize_256(Self::light())
    }

    fn quantize_256(mut theme: Self) -> Self {
        use super::terminal_palette::{StdoutColorLevel, best_color_for};

        let quantize = |color| match color {
            Color::Rgb(red, green, blue) => {
                best_color_for(StdoutColorLevel::Ansi256, (red, green, blue))
            }
            color => color,
        };
        theme.accent = quantize(theme.accent);
        theme.selected_bg = quantize(theme.selected_bg);
        theme.selected_fg = quantize(theme.selected_fg);
        theme.gutter = quantize(theme.gutter);
        theme.gutter_frozen = quantize(theme.gutter_frozen);
        theme.success = quantize(theme.success);
        theme.warn = quantize(theme.warn);
        theme.error = quantize(theme.error);
        theme.quote = quantize(theme.quote);
        theme.link = quantize(theme.link);
        theme.command = quantize(theme.command);
        theme.md_heading = quantize(theme.md_heading);
        theme.md_code = quantize(theme.md_code);
        theme.md_link = quantize(theme.md_link);
        theme.md_blockquote = quantize(theme.md_blockquote);
        theme.md_list_marker = quantize(theme.md_list_marker);
        theme.diff_add_fg = quantize(theme.diff_add_fg);
        theme.diff_add_bg = quantize(theme.diff_add_bg);
        theme.diff_del_fg = quantize(theme.diff_del_fg);
        theme.diff_del_bg = quantize(theme.diff_del_bg);
        theme.diff_hunk = quantize(theme.diff_hunk);
        theme.stall_warn = quantize(theme.stall_warn);
        theme.stall_error = quantize(theme.stall_error);
        theme
    }

    /// 16-colour dark fallback for terminals that cannot faithfully render
    /// truecolor (notably a number of remote shells and multiplexers).
    pub fn dark_ansi() -> Self {
        let mut theme = Self::dark();
        theme.accent = Color::LightBlue;
        theme.selected_bg = Color::DarkGray;
        theme.selected_fg = Color::White;
        theme.gutter = Color::LightGreen;
        theme.gutter_frozen = Color::DarkGray;
        theme.success = Color::LightGreen;
        theme.warn = Color::Yellow;
        theme.error = Color::LightRed;
        theme.quote = Color::LightGreen;
        theme.link = Color::Cyan;
        theme.path_dim = Color::DarkGray;
        theme.path_file = Color::White;
        theme.command = Color::Cyan;
        theme.md_heading = Color::LightBlue;
        theme.md_code = Color::LightBlue;
        theme.md_link = Color::Cyan;
        theme.md_blockquote = Color::LightGreen;
        theme.md_list_marker = Color::LightBlue;
        // A genuine 16-colour palette cannot express a restrained tinted
        // surface. Painting a complete row with `Green`/`Red` is much more
        // disruptive than losing the tint, so degrade to a neutral surface
        // while retaining direction in foreground and the +/- marker.
        theme.diff_add_fg = Color::LightGreen;
        theme.diff_add_bg = Color::Black;
        theme.diff_del_fg = Color::LightRed;
        theme.diff_del_bg = Color::Black;
        theme.diff_hunk = Color::Cyan;
        theme.diff_context = Color::DarkGray;
        theme.stall_warn = Color::Yellow;
        theme.stall_error = Color::LightRed;
        theme
    }

    /// 16-colour light fallback. Explicit foregrounds avoid the accidental
    /// low-contrast RGB down-conversion that prompted this profile.
    pub fn light_ansi() -> Self {
        let mut theme = Self::light();
        theme.accent = Color::Blue;
        theme.selected_bg = Color::Gray;
        theme.selected_fg = Color::Black;
        theme.gutter = Color::LightGreen;
        theme.gutter_frozen = Color::DarkGray;
        theme.success = Color::LightGreen;
        theme.warn = Color::Yellow;
        theme.error = Color::LightRed;
        theme.quote = Color::LightGreen;
        theme.link = Color::Blue;
        theme.path_dim = Color::DarkGray;
        theme.path_file = Color::Black;
        theme.command = Color::Blue;
        theme.md_heading = Color::Blue;
        theme.md_code = Color::Blue;
        theme.md_link = Color::Blue;
        theme.md_blockquote = Color::LightGreen;
        theme.md_list_marker = Color::Blue;
        // See `dark_ansi`: on a light 16-colour terminal the default-like
        // neutral surface is white, with direction carried by readable text.
        theme.diff_add_fg = Color::Green;
        theme.diff_add_bg = Color::White;
        theme.diff_del_fg = Color::Red;
        theme.diff_del_bg = Color::White;
        theme.diff_hunk = Color::Blue;
        theme.diff_context = Color::DarkGray;
        theme.stall_warn = Color::Yellow;
        theme.stall_error = Color::LightRed;
        theme
    }

    /// Honor the `NO_COLOR` convention. The structure, labels, emphasis and
    /// keyboard hints remain; only colour is removed.
    pub fn plain() -> Self {
        Self {
            is_light: false,
            fg: Color::Reset,
            dim: Color::Reset,
            accent: Color::Reset,
            selected_bg: Color::Reset,
            selected_fg: Color::Reset,
            gutter: Color::Reset,
            gutter_frozen: Color::Reset,
            success: Color::Reset,
            warn: Color::Reset,
            error: Color::Reset,
            quote: Color::Reset,
            link: Color::Reset,
            path_dim: Color::Reset,
            path_file: Color::Reset,
            command: Color::Reset,
            md_heading: Color::Reset,
            md_code: Color::Reset,
            md_link: Color::Reset,
            md_blockquote: Color::Reset,
            md_list_marker: Color::Reset,
            diff_add_fg: Color::Reset,
            diff_add_bg: Color::Reset,
            diff_del_fg: Color::Reset,
            diff_del_bg: Color::Reset,
            diff_hunk: Color::Reset,
            diff_context: Color::Reset,
            stall_warn: Color::Reset,
            stall_error: Color::Reset,
        }
    }

    fn for_profile(profile: ThemeProfile) -> Self {
        match profile {
            ThemeProfile::Auto => Self::auto(),
            ThemeProfile::Dark => Self::dark(),
            ThemeProfile::Light => Self::light(),
            ThemeProfile::DarkAnsi => Self::dark_ansi(),
            ThemeProfile::LightAnsi => Self::light_ansi(),
            ThemeProfile::Plain => Self::plain(),
        }
    }

    /// Select a preset automatically based on terminal background.
    /// Falls back to `dark` when no signal is available.
    pub fn auto() -> Self {
        let light = is_light_background();
        if supports_truecolor() {
            return if light { Self::light() } else { Self::dark() };
        }
        if super::terminal_palette::stdout_color_level()
            == super::terminal_palette::StdoutColorLevel::Ansi256
        {
            return if light {
                Self::light_256()
            } else {
                Self::dark_256()
            };
        }
        if light {
            Self::light_ansi()
        } else {
            Self::dark_ansi()
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

    /// Style for the directory portion of a file path.
    pub fn path_dim_style(&self) -> Style {
        Style::default().fg(self.path_dim)
    }

    /// Style for the filename portion of a file path.
    pub fn path_file_style(&self) -> Style {
        Style::default().fg(self.path_file)
    }

    /// Style for shell command text (the body after `$ `).
    pub fn command_style(&self) -> Style {
        Style::default().fg(self.command)
    }

    // ── Markdown helper styles ───────────────────────────────────────────
    pub fn md_heading_style(&self) -> Style {
        Style::default().fg(self.md_heading)
    }
    pub fn md_code_style(&self) -> Style {
        Style::default().fg(self.md_code)
    }
    pub fn md_link_style(&self) -> Style {
        Style::default().fg(self.md_link)
    }
    pub fn md_blockquote_style(&self) -> Style {
        Style::default().fg(self.md_blockquote)
    }
    pub fn md_list_marker_style(&self) -> Style {
        Style::default().fg(self.md_list_marker)
    }

    // ── Diff helper styles ───────────────────────────────────────────────
    pub fn diff_add_style(&self) -> Style {
        Style::default().fg(self.diff_add_fg).bg(self.diff_add_bg)
    }
    pub fn diff_del_style(&self) -> Style {
        Style::default().fg(self.diff_del_fg).bg(self.diff_del_bg)
    }
    pub fn diff_context_style(&self) -> Style {
        Style::default().fg(self.diff_context)
    }
    pub fn diff_hunk_style(&self) -> Style {
        Style::default().fg(self.diff_hunk)
    }

    // ── Status indicator helper styles ───────────────────────────────────
    pub fn stall_warn_style(&self) -> Style {
        Style::default().fg(self.stall_warn)
    }
    pub fn stall_error_style(&self) -> Style {
        Style::default().fg(self.stall_error)
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

fn supports_truecolor() -> bool {
    if super::terminal_palette::stdout_color_level()
        == super::terminal_palette::StdoutColorLevel::TrueColor
    {
        return true;
    }
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return true;
    }

    matches!(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        Some("iTerm.app" | "WezTerm" | "Apple_Terminal" | "vscode")
    )
}

fn perceived_lightness(r: u8, g: u8, b: u8) -> f32 {
    // sRGB → perceived luma (quick approximation).
    (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
}

pub(crate) fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (128, 128, 128),
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Process-wide theme, chosen once at first access. Tests that need a
/// specific theme should call [`set_for_tests`] *before* any `current()`.
pub(crate) fn current() -> &'static Theme {
    THEME.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return Theme::plain();
        }
        let profile = std::env::var("ASTRA_TUI_THEME")
            .ok()
            .as_deref()
            .and_then(ThemeProfile::parse)
            .unwrap_or(ThemeProfile::Auto);
        Theme::for_profile(profile)
    })
}

#[cfg(test)]
pub(crate) fn set_for_tests(theme: Theme) {
    let _ = THEME.set(theme);
}

#[cfg(test)]
mod tests {
    use super::{Theme, ThemeProfile, color_to_rgb, current, perceived_lightness};
    use ratatui::style::Color;

    #[test]
    fn dark_and_light_are_distinct() {
        assert!(!Theme::dark().is_light);
        assert!(Theme::light().is_light);
        assert_ne!(Theme::dark().accent, Theme::light().accent);
    }

    #[test]
    fn ansi_profiles_use_only_terminal_palette_colours() {
        for theme in [Theme::dark_ansi(), Theme::light_ansi()] {
            assert!(
                [
                    theme.accent,
                    theme.selected_bg,
                    theme.selected_fg,
                    theme.gutter,
                    theme.md_heading,
                    theme.diff_add_fg,
                    theme.diff_del_fg,
                ]
                .iter()
                .all(|color| !matches!(color, Color::Rgb(_, _, _))),
                "ANSI fallback must not leak RGB values: {theme:?}"
            );
        }
    }

    #[test]
    fn indexed_theme_uses_quiet_edit_surfaces() {
        let dark = Theme::dark_256();
        assert_eq!(dark.diff_add_bg, Color::Indexed(234), "{dark:?}");
        assert_eq!(dark.diff_del_bg, Color::Indexed(234), "{dark:?}");
        assert_ne!(dark.diff_add_fg, dark.diff_del_fg, "{dark:?}");

        for theme in [dark, Theme::light_256()] {
            assert!(matches!(theme.diff_add_bg, Color::Indexed(_)), "{theme:?}");
            assert!(matches!(theme.diff_del_bg, Color::Indexed(_)), "{theme:?}");
            assert_ne!(theme.diff_add_bg, Color::Reset, "{theme:?}");
            assert_ne!(theme.diff_del_bg, Color::Reset, "{theme:?}");
        }
    }

    #[test]
    fn profile_parser_accepts_explicit_terminal_safe_choices() {
        assert_eq!(
            ThemeProfile::parse("dark-ansi"),
            Some(ThemeProfile::DarkAnsi)
        );
        assert_eq!(
            ThemeProfile::parse("ansi-light"),
            Some(ThemeProfile::LightAnsi)
        );
        assert_eq!(ThemeProfile::parse("no-color"), Some(ThemeProfile::Plain));
        assert_eq!(ThemeProfile::parse("not-a-theme"), None);
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
    fn dark_palette_uses_emerald_activity_and_reserves_red_for_failure() {
        let theme = Theme::dark();
        let (ar, ag, ab) = color_to_rgb(theme.accent);
        let (gr, gg, gb) = color_to_rgb(theme.gutter);
        let (er, eg, eb) = color_to_rgb(theme.error);

        assert!(
            ab > ar && ab > ag,
            "accent should read as a cool focus color: {theme:?}"
        );
        assert!(
            gg > gr && gg > gb,
            "live activity should use the high-salience emerald gutter: {theme:?}"
        );
        assert!(
            er > eg && er > eb,
            "only the error role should carry a warm failure hue: {theme:?}"
        );
        assert_ne!(theme.error, theme.accent);
        assert_ne!(theme.error, theme.warn);
    }

    #[test]
    fn diff_rows_stay_restrained_at_every_terminal_color_level() {
        let dark = Theme::dark();
        let (_, add_g, _) = color_to_rgb(dark.diff_add_bg);
        let (del_r, _, _) = color_to_rgb(dark.diff_del_bg);
        assert!(
            add_g < 64,
            "dark add surface must stay restrained: {dark:?}"
        );
        assert!(
            del_r < 80,
            "dark delete surface must stay restrained: {dark:?}"
        );

        let dark_ansi = Theme::dark_ansi();
        assert_eq!(dark_ansi.diff_add_bg, Color::Black);
        assert_eq!(dark_ansi.diff_del_bg, Color::Black);
        assert_eq!(dark_ansi.diff_add_fg, Color::LightGreen);
        assert_eq!(dark_ansi.diff_del_fg, Color::LightRed);

        let light_ansi = Theme::light_ansi();
        assert_eq!(light_ansi.diff_add_bg, Color::White);
        assert_eq!(light_ansi.diff_del_bg, Color::White);
        assert_eq!(light_ansi.diff_add_fg, Color::Green);
        assert_eq!(light_ansi.diff_del_fg, Color::Red);
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
