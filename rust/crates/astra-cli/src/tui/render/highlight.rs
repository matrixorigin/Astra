use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::syntax::extra_newlines;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName, extra as extra_themes};

use super::super::color::is_light;
use super::super::terminal_palette::default_bg;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<RwLock<Theme>> = OnceLock::new();
static THEME_OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
static ASTRA_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

fn syntax_set() -> &'static SyntaxSet {
    // Use the pre-built binary directly.  Calling into_builder().build() would
    // decompress every syntax definition, re-link all context references, and
    // re-compress everything — O(300+ syntaxes) of flate2 work that is both
    // slow and unnecessary.  extra_newlines() already contains plain-text
    // syntax (it is based on syntect's defaults), so add_plain_text_syntax()
    // is redundant too.
    SYNTAX_SET.get_or_init(extra_newlines)
}

fn default_theme() -> Theme {
    let is_light_bg = default_bg().map_or(false, is_light);
    let themes = extra_themes();
    let name = if is_light_bg {
        EmbeddedThemeName::CatppuccinLatte
    } else {
        EmbeddedThemeName::CatppuccinMocha
    };
    themes.get(name).clone()
}

fn current_theme() -> &'static RwLock<Theme> {
    THEME.get_or_init(|| {
        let theme = THEME_OVERRIDE
            .get()
            .and_then(|o| o.as_ref())
            .and_then(|name| resolve_theme_by_name(name))
            .unwrap_or_else(default_theme);
        RwLock::new(theme)
    })
}

pub(crate) fn resolve_theme_by_name(name: &str) -> Option<Theme> {
    for theme_name in EmbeddedLazyThemeSet::theme_names() {
        if theme_name.as_name().eq_ignore_ascii_case(name) {
            return Some(extra_themes().get(*theme_name).clone());
        }
    }
    if let Some(home) = ASTRA_HOME.get().and_then(|h| h.as_ref()) {
        let path = home.join("themes").join(format!("{name}.tmTheme"));
        if path.exists() {
            if let Ok(theme) = ThemeSet::get_theme(&path) {
                return Some(theme);
            }
        }
    }
    None
}

#[allow(dead_code)]
pub(crate) fn set_theme_override(
    name: Option<String>,
    astra_home: Option<PathBuf>,
) -> Option<String> {
    let _ = ASTRA_HOME.set(astra_home);
    let resolved = name.as_ref().and_then(|n| resolve_theme_by_name(n));
    let accepted = if resolved.is_some() { name } else { None };
    let _ = THEME_OVERRIDE.set(accepted.clone());
    accepted
}

#[allow(dead_code)]
pub(crate) fn set_syntax_theme(theme: Theme) {
    if let Ok(mut t) = current_theme().write() {
        *t = theme;
    }
}

#[allow(dead_code)]
pub(crate) fn current_syntax_theme() -> Theme {
    current_theme().read().map(|t| t.clone()).unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) fn configured_theme_name() -> Option<String> {
    THEME_OVERRIDE.get().and_then(|o| o.clone())
}

#[allow(dead_code)]
pub(crate) fn list_available_themes(astra_home: Option<&PathBuf>) -> Vec<String> {
    let mut names: Vec<String> = EmbeddedLazyThemeSet::theme_names()
        .iter()
        .map(|t| t.as_name().to_string())
        .collect();
    if let Some(home) = astra_home {
        let themes_dir = home.join("themes");
        if themes_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "tmTheme") {
                        if let Some(stem) = path.file_stem() {
                            names.push(stem.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let lang = match lang.to_lowercase().as_str() {
        "csharp" | "c#" => "c#".to_string(),
        "golang" => "go".to_string(),
        "python3" => "python".to_string(),
        "shell" | "sh" | "zsh" => "bash".to_string(),
        "yml" => "yaml".to_string(),
        "js" => "javascript".to_string(),
        "ts" => "typescript".to_string(),
        "rs" => "rust".to_string(),
        "rb" => "ruby".to_string(),
        "py" => "python".to_string(),
        "md" => "markdown".to_string(),
        other => other.to_string(),
    };
    ss.find_syntax_by_token(&lang)
        .or_else(|| ss.find_syntax_by_name(&lang))
        .or_else(|| ss.find_syntax_by_extension(&lang))
}

fn convert_syntect_color(
    syntect::highlighting::Color { r, g, b, a }: syntect::highlighting::Color,
) -> Option<Color> {
    if a == 0x00 {
        match r {
            0 => Some(Color::Black),
            1 => Some(Color::Red),
            2 => Some(Color::Green),
            3 => Some(Color::Yellow),
            4 => Some(Color::Blue),
            5 => Some(Color::Magenta),
            6 => Some(Color::Cyan),
            7 => Some(Color::Gray),
            8 => Some(Color::DarkGray),
            9 => Some(Color::LightRed),
            10 => Some(Color::LightGreen),
            11 => Some(Color::LightYellow),
            12 => Some(Color::LightBlue),
            13 => Some(Color::LightMagenta),
            14 => Some(Color::LightCyan),
            15 => Some(Color::White),
            n => Some(Color::Indexed(n)),
        }
    } else if a == 0x01 {
        None
    } else {
        Some(Color::Rgb(r, g, b))
    }
}

pub(crate) fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    let line_count = code.lines().count();
    if exceeds_highlight_limits(code.len(), line_count) {
        return code.lines().map(|l| Line::raw(l.to_string())).collect();
    }

    let syntax = match find_syntax(lang) {
        Some(s) => s,
        None => return code.lines().map(|l| Line::raw(l.to_string())).collect(),
    };

    let theme = match current_theme().read() {
        Ok(t) => t.clone(),
        Err(_) => return code.lines().map(|l| Line::raw(l.to_string())).collect(),
    };

    let mut highlighter = syntect::easy::HighlightLines::new(syntax, &theme);
    let ss = syntax_set();

    code.lines()
        .map(|line| {
            let regions = highlighter.highlight_line(line, ss).unwrap_or_default();

            let spans: Vec<Span<'static>> = regions
                .into_iter()
                .map(|(hl_style, text)| {
                    let mut style = Style::default();
                    if let Some(fg) = convert_syntect_color(hl_style.foreground) {
                        style = style.fg(fg);
                    }
                    if hl_style.font_style.contains(FontStyle::BOLD) {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    let text = text.trim_end_matches('\n').trim_end_matches('\r');
                    Span::styled(text.to_string(), style)
                })
                .collect();

            if spans.is_empty() {
                Line::raw(String::new())
            } else {
                Line::from(spans)
            }
        })
        .collect()
}

#[allow(dead_code)]
pub(crate) fn highlight_bash_to_lines(script: &str) -> Vec<Line<'static>> {
    highlight_code_to_lines(script, "bash")
}
