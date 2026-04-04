//! Centralized theme and color configuration for terminal output.
//!
//! All prompt strings and semantic color helpers live here so that:
//! - Colors are easy to change or theme
//! - Non-TTY environments can disable colors in one place
//! - Readline prompts use only ASCII text (ANSI codes are safe for cursor math)

#![allow(dead_code)]

use crossterm::style::Stylize;
use std::borrow::Cow;

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

/// Success indicator: green ✓
pub fn icon_ok() -> String {
    "✓".green().to_string()
}

/// Error indicator: red ✗
pub fn icon_err() -> String {
    "✗".red().to_string()
}

/// Warning indicator: yellow ⚠
pub fn icon_warn() -> String {
    "⚠".yellow().to_string()
}

/// Info indicator: cyan ℹ
pub fn icon_info() -> String {
    "ℹ".cyan().to_string()
}

// ── Semantic text styles ──────────────────────────────────────────────────

/// Style a header/title (bold)
pub fn header(text: &str) -> String {
    text.bold().to_string()
}

/// Style a section label (cyan bold)
pub fn section(text: &str) -> String {
    text.cyan().bold().to_string()
}

/// Style subtle/secondary text (dim)
pub fn dim(text: &str) -> String {
    text.dim().to_string()
}

/// Style an error message (red)
pub fn error(text: &str) -> String {
    text.red().to_string()
}

/// Style a success message (green)
pub fn success(text: &str) -> String {
    text.green().to_string()
}

/// Style a warning message (yellow)
pub fn warning(text: &str) -> String {
    text.yellow().to_string()
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
}
