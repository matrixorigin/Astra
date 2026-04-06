//! Centralized theme and color configuration for terminal output.
//!
//! All prompt strings and semantic color helpers live here so that:
//! - Colors are easy to change or theme
//! - Non-TTY environments can disable colors in one place
//! - Readline prompts use only ASCII text (ANSI codes are safe for cursor math)
//!
//! ## Custom Themes
//!
//! Users can define custom themes in `~/.astra/styles/<name>.yaml`. The `/style`
//! command switches between built-in and user-defined themes at runtime.

#![allow(dead_code)]

use crossterm::style::Stylize;
use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::RwLock;

// ── Theme Configuration ───────────────────────────────────────────────────

/// Color name from the terminal's 16-color palette.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    DarkGrey,
    DarkRed,
    DarkGreen,
    DarkYellow,
    DarkBlue,
    DarkMagenta,
    DarkCyan,
    Grey,
}

impl ThemeColor {
    fn to_crossterm(self) -> crossterm::style::Color {
        use crossterm::style::Color;
        match self {
            Self::Black => Color::Black,
            Self::Red => Color::Red,
            Self::Green => Color::Green,
            Self::Yellow => Color::Yellow,
            Self::Blue => Color::Blue,
            Self::Magenta => Color::Magenta,
            Self::Cyan => Color::Cyan,
            Self::White => Color::White,
            Self::DarkGrey => Color::DarkGrey,
            Self::DarkRed => Color::DarkRed,
            Self::DarkGreen => Color::DarkGreen,
            Self::DarkYellow => Color::DarkYellow,
            Self::DarkBlue => Color::DarkBlue,
            Self::DarkMagenta => Color::DarkMagenta,
            Self::DarkCyan => Color::DarkCyan,
            Self::Grey => Color::Grey,
        }
    }
}

/// Configurable theme for terminal output colors.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ThemeConfig {
    /// Theme display name
    pub name: String,
    /// Prompt color (default: cyan)
    #[serde(default = "default_cyan")]
    pub prompt: ThemeColor,
    /// Success messages (default: green)
    #[serde(default = "default_green")]
    pub success: ThemeColor,
    /// Error messages (default: red)
    #[serde(default = "default_red")]
    pub error: ThemeColor,
    /// Warning messages (default: yellow)
    #[serde(default = "default_yellow")]
    pub warning: ThemeColor,
    /// Info/accent color (default: cyan)
    #[serde(default = "default_cyan")]
    pub info: ThemeColor,
    /// Section headers (default: cyan)
    #[serde(default = "default_cyan")]
    pub section_color: ThemeColor,
    /// Tool call display (default: blue)
    #[serde(default = "default_blue")]
    pub tool: ThemeColor,
    /// Use bold for headers
    #[serde(default = "default_true")]
    pub bold_headers: bool,
    /// Use dim for secondary text
    #[serde(default = "default_true")]
    pub dim_secondary: bool,
}

fn default_cyan() -> ThemeColor {
    ThemeColor::Cyan
}
fn default_green() -> ThemeColor {
    ThemeColor::Green
}
fn default_red() -> ThemeColor {
    ThemeColor::Red
}
fn default_yellow() -> ThemeColor {
    ThemeColor::Yellow
}
fn default_blue() -> ThemeColor {
    ThemeColor::Blue
}
fn default_true() -> bool {
    true
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            prompt: ThemeColor::Cyan,
            success: ThemeColor::Green,
            error: ThemeColor::Red,
            warning: ThemeColor::Yellow,
            info: ThemeColor::Cyan,
            section_color: ThemeColor::Cyan,
            tool: ThemeColor::Blue,
            bold_headers: true,
            dim_secondary: true,
        }
    }
}

/// Global active theme (thread-safe, swappable at runtime).
static ACTIVE_THEME: OnceLock<RwLock<ThemeConfig>> = OnceLock::new();

fn active_theme() -> &'static RwLock<ThemeConfig> {
    ACTIVE_THEME.get_or_init(|| RwLock::new(ThemeConfig::default()))
}

/// Get a clone of the current active theme.
pub fn current_theme() -> ThemeConfig {
    active_theme().read().unwrap().clone()
}

/// Get the current theme name.
pub fn current_theme_name() -> String {
    active_theme().read().unwrap().name.clone()
}

/// Set the active theme.
pub fn set_theme(theme: ThemeConfig) {
    *active_theme().write().unwrap() = theme;
}

/// List available built-in themes.
pub fn builtin_themes() -> Vec<ThemeConfig> {
    vec![
        ThemeConfig::default(),
        ThemeConfig {
            name: "minimal".to_string(),
            prompt: ThemeColor::White,
            success: ThemeColor::White,
            error: ThemeColor::Red,
            warning: ThemeColor::Yellow,
            info: ThemeColor::White,
            section_color: ThemeColor::White,
            tool: ThemeColor::White,
            bold_headers: true,
            dim_secondary: true,
        },
        ThemeConfig {
            name: "colorful".to_string(),
            prompt: ThemeColor::Magenta,
            success: ThemeColor::Green,
            error: ThemeColor::Red,
            warning: ThemeColor::Yellow,
            info: ThemeColor::Cyan,
            section_color: ThemeColor::Blue,
            tool: ThemeColor::Magenta,
            bold_headers: true,
            dim_secondary: false,
        },
        ThemeConfig {
            name: "high-contrast".to_string(),
            prompt: ThemeColor::White,
            success: ThemeColor::Green,
            error: ThemeColor::Red,
            warning: ThemeColor::Yellow,
            info: ThemeColor::White,
            section_color: ThemeColor::White,
            tool: ThemeColor::Yellow,
            bold_headers: true,
            dim_secondary: false,
        },
    ]
}

/// Load user themes from `~/.astra/styles/`.
pub fn load_user_themes() -> Vec<ThemeConfig> {
    let dir = match dirs::home_dir() {
        Some(h) => h.join(".astra").join("styles"),
        None => return Vec::new(),
    };
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut themes = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .map_or(false, |ext| ext == "yaml" || ext == "yml")
            {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    match serde_yaml::from_str::<ThemeConfig>(&contents) {
                        Ok(theme) => themes.push(theme),
                        Err(e) => {
                            eprintln!(
                                "  ⚠ Failed to parse theme {}: {e}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
    }
    themes
}

/// Find and activate a theme by name (searches built-in first, then user).
pub fn activate_theme_by_name(name: &str) -> Result<(), String> {
    // Check built-in
    for t in builtin_themes() {
        if t.name.eq_ignore_ascii_case(name) {
            set_theme(t);
            return Ok(());
        }
    }
    // Check user themes
    for t in load_user_themes() {
        if t.name.eq_ignore_ascii_case(name) {
            set_theme(t);
            return Ok(());
        }
    }
    Err(format!("Theme '{name}' not found"))
}

// ── Readline prompts ──────────────────────────────────────────────────────
//
// IMPORTANT: Prompt TEXT must be ASCII-only. Unicode characters with ambiguous
// display widths (⏸ U+23F8, 🔄 U+1F504, · U+00B7) break rustyline's cursor
// tracking for CJK input. ANSI color codes (\x1b[...m) are safe — rustyline
// treats them as width=0.

/// Default prompt: cyan bold `>`
pub const PROMPT_DEFAULT: &str = "\x1b[1;36m>\x1b[0m ";

/// Plan mode prompt: yellow bold `plan>`
pub const PROMPT_PLAN: &str = "\x1b[1;33mplan>\x1b[0m ";

/// Paused plan execution prompt: yellow bold `pause>`
pub const PROMPT_PAUSE: &str = "\x1b[1;33mpause>\x1b[0m ";

/// Background plan running prompt: cyan bold `bg>`
pub const PROMPT_BG: &str = "\x1b[1;36mbg>\x1b[0m ";

/// Chat plan-only mode prompt: yellow bold `plan.`
pub const PROMPT_PLAN_ONLY: &str = "\x1b[1;33mplan.\x1b[0m ";

// ── Semantic icons ────────────────────────────────────────────────────────

/// Apply a ThemeColor to a string.
fn styled(text: &str, color: ThemeColor) -> String {
    use crossterm::style::StyledContent;
    let c = color.to_crossterm();
    StyledContent::new(crossterm::style::ContentStyle::new().with(c), text).to_string()
}

/// Success indicator: ✓ in theme success color
pub fn icon_ok() -> String {
    styled("✓", current_theme().success)
}

/// Error indicator: ✗ in theme error color
pub fn icon_err() -> String {
    styled("✗", current_theme().error)
}

/// Warning indicator: ⚠ in theme warning color
pub fn icon_warn() -> String {
    styled("⚠", current_theme().warning)
}

/// Info indicator: ℹ in theme info color
pub fn icon_info() -> String {
    styled("ℹ", current_theme().info)
}

// ── Semantic text styles ──────────────────────────────────────────────────

/// Style a header/title (bold, optionally theme-colored)
pub fn header(text: &str) -> String {
    let t = current_theme();
    if t.bold_headers {
        text.bold().to_string()
    } else {
        text.to_string()
    }
}

/// Style a section label (theme section_color, bold)
pub fn section(text: &str) -> String {
    let t = current_theme();
    let base = styled(text, t.section_color);
    if t.bold_headers {
        base // already colored, crossterm doesn't chain easily — acceptable
    } else {
        base
    }
}

/// Style subtle/secondary text (dim if theme enables it)
pub fn dim(text: &str) -> String {
    if current_theme().dim_secondary {
        text.dim().to_string()
    } else {
        text.to_string()
    }
}

/// Style an error message (theme error color)
pub fn error(text: &str) -> String {
    styled(text, current_theme().error)
}

/// Style a success message (theme success color)
pub fn success(text: &str) -> String {
    styled(text, current_theme().success)
}

/// Style a warning message (theme warning color)
pub fn warning(text: &str) -> String {
    styled(text, current_theme().warning)
}

// ── Terminal control sequences ────────────────────────────────────────────

/// Move cursor up one line and clear it.
pub const CURSOR_UP_CLEAR: &str = "\x1b[A\x1b[2K";

// ── Strip ANSI for non-TTY ───────────────────────────────────────────────

/// Strip ANSI escape codes from a string (for logging, file output, etc.)
///
/// Handles CSI sequences (`\x1b[...X` where X is any letter `@`–`~`),
/// OSC sequences (`\x1b]...BEL/ST`), and simple two-byte sequences (`\x1bX`).
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    // Fast path: no escape char means no ANSI codes
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: \x1b[ ... <final byte 0x40–0x7E>
                    chars.next(); // consume '['
                    for inner in chars.by_ref() {
                        if ('@'..='~').contains(&inner) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence: \x1b] ... (BEL or ST)
                    chars.next();
                    for inner in chars.by_ref() {
                        if inner == '\x07' {
                            break;
                        }
                        if inner == '\x1b' {
                            chars.next(); // consume '\\' of ST
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Two-byte sequence (e.g. \x1b= , \x1b> )
                    chars.next();
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_constants_are_ascii_text() {
        // Verify prompt TEXT (between ANSI codes) is ASCII-only
        for prompt in [
            PROMPT_DEFAULT,
            PROMPT_PLAN,
            PROMPT_PAUSE,
            PROMPT_BG,
            PROMPT_PLAN_ONLY,
        ] {
            let text = strip_ansi(prompt);
            assert!(
                text.is_ascii(),
                "Prompt text must be ASCII-only, got: {text:?}"
            );
        }
    }

    #[test]
    fn strip_ansi_no_codes() {
        assert_eq!(strip_ansi("hello"), "hello");
    }

    #[test]
    fn strip_ansi_with_codes() {
        assert_eq!(strip_ansi("\x1b[1;36m>\x1b[0m "), "> ");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m text"), "red text");
    }

    #[test]
    fn strip_ansi_csi_non_sgr() {
        // Cursor movement (\x1b[2J = clear screen, \x1b[H = cursor home)
        assert_eq!(strip_ansi("\x1b[2Jtext\x1b[Hmore"), "textmore");
        // Cursor up (\x1b[A)
        assert_eq!(strip_ansi("before\x1b[Aafter"), "beforeafter");
    }

    #[test]
    fn strip_ansi_osc() {
        // OSC title set: \x1b]0;title\x07
        assert_eq!(strip_ansi("\x1b]0;My Title\x07visible"), "visible");
    }

    #[test]
    fn icons_are_non_empty() {
        assert!(!icon_ok().is_empty());
        assert!(!icon_err().is_empty());
        assert!(!icon_warn().is_empty());
    }

    #[test]
    fn builtin_themes_all_have_names() {
        let themes = builtin_themes();
        assert!(themes.len() >= 4);
        for t in &themes {
            assert!(!t.name.is_empty());
        }
    }

    #[test]
    fn activate_and_read_theme() {
        // Switch to high-contrast
        activate_theme_by_name("high-contrast").unwrap();
        assert_eq!(current_theme_name(), "high-contrast");
        // Switch back to default
        activate_theme_by_name("default").unwrap();
        assert_eq!(current_theme_name(), "default");
    }

    #[test]
    fn activate_unknown_theme_returns_error() {
        assert!(activate_theme_by_name("nonexistent").is_err());
    }

    #[test]
    fn theme_aware_functions_produce_output() {
        // Just verify they don't panic and produce non-empty strings
        assert!(!header("Title").is_empty());
        assert!(!section("Section").is_empty());
        assert!(!dim("subtle").is_empty());
        assert!(!error("err").is_empty());
        assert!(!success("ok").is_empty());
        assert!(!warning("warn").is_empty());
        assert!(!icon_info().is_empty());
    }
}
