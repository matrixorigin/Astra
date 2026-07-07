use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::super::color::is_light;
use super::super::terminal_palette::default_bg;

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Go,
    Java,
    CLike,
    Ruby,
    Shell,
    Json,
    Yaml,
}

impl Language {
    fn from_token(lang: &str) -> Option<Self> {
        match lang.to_lowercase().as_str() {
            "rs" | "rust" => Some(Self::Rust),
            "js" | "jsx" | "javascript" => Some(Self::JavaScript),
            "ts" | "tsx" | "typescript" => Some(Self::TypeScript),
            "py" | "python" | "python3" => Some(Self::Python),
            "go" | "golang" => Some(Self::Go),
            "java" => Some(Self::Java),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "c++" | "csharp" | "c#" => {
                Some(Self::CLike)
            }
            "rb" | "ruby" => Some(Self::Ruby),
            "bash" | "sh" | "shell" | "zsh" => Some(Self::Shell),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            _ => None,
        }
    }

    fn hash_starts_comment(self) -> bool {
        matches!(self, Self::Python | Self::Ruby | Self::Shell | Self::Yaml)
    }

    fn slash_starts_comment(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::JavaScript
                | Self::TypeScript
                | Self::Go
                | Self::Java
                | Self::CLike
        )
    }

    fn allows_backtick_strings(self) -> bool {
        matches!(self, Self::JavaScript | Self::TypeScript | Self::Go | Self::Shell)
    }
}

#[derive(Clone, Copy)]
struct Palette {
    keyword: Color,
    type_name: Color,
    string: Color,
    comment: Color,
    number: Color,
    punctuation: Color,
}

fn palette() -> Palette {
    if default_bg().is_some_and(is_light) {
        Palette {
            keyword: Color::Rgb(148, 40, 148),
            type_name: Color::Rgb(0, 92, 145),
            string: Color::Rgb(22, 115, 46),
            comment: Color::Rgb(100, 116, 139),
            number: Color::Rgb(135, 89, 0),
            punctuation: Color::Rgb(100, 116, 139),
        }
    } else {
        Palette {
            keyword: Color::Rgb(199, 146, 234),
            type_name: Color::Rgb(130, 170, 255),
            string: Color::Rgb(173, 219, 103),
            comment: Color::Rgb(117, 132, 161),
            number: Color::Rgb(247, 140, 108),
            punctuation: Color::Rgb(137, 151, 177),
        }
    }
}

fn exceeds_highlight_limits(total_bytes: usize, total_lines: usize) -> bool {
    total_bytes > MAX_HIGHLIGHT_BYTES || total_lines > MAX_HIGHLIGHT_LINES
}

fn style(color: Color) -> Style {
    Style::default().fg(color)
}

fn bold_style(color: Color) -> Style {
    style(color).add_modifier(Modifier::BOLD)
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_keyword(language: Language, token: &str) -> bool {
    match language {
        Language::Rust => matches!(
            token,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        Language::JavaScript | Language::TypeScript => matches!(
            token,
            "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "else"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "if"
                | "import"
                | "in"
                | "interface"
                | "let"
                | "new"
                | "null"
                | "return"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "type"
                | "undefined"
                | "var"
                | "while"
        ),
        Language::Python => matches!(
            token,
            "and" | "as"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "elif"
                | "else"
                | "except"
                | "False"
                | "finally"
                | "for"
                | "from"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "None"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "True"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        Language::Go => matches!(
            token,
            "break" | "case"
                | "chan"
                | "const"
                | "continue"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        Language::Java | Language::CLike => matches!(
            token,
            "auto" | "bool"
                | "break"
                | "case"
                | "catch"
                | "char"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "do"
                | "double"
                | "else"
                | "enum"
                | "false"
                | "final"
                | "float"
                | "for"
                | "if"
                | "int"
                | "long"
                | "namespace"
                | "new"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "short"
                | "static"
                | "struct"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "using"
                | "void"
                | "while"
        ),
        Language::Ruby => matches!(
            token,
            "begin" | "break"
                | "case"
                | "class"
                | "def"
                | "do"
                | "else"
                | "elsif"
                | "end"
                | "ensure"
                | "false"
                | "for"
                | "if"
                | "module"
                | "next"
                | "nil"
                | "rescue"
                | "return"
                | "self"
                | "then"
                | "true"
                | "unless"
                | "until"
                | "when"
                | "while"
                | "yield"
        ),
        Language::Shell => matches!(
            token,
            "case" | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "export"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "local"
                | "then"
                | "while"
        ),
        Language::Json | Language::Yaml => {
            matches!(token, "true" | "false" | "null" | "True" | "False" | "Null")
        }
    }
}

fn consume_string(line: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    let mut idx = start + quote.len_utf8();
    while idx < line.len() {
        let Some(ch) = line[idx..].chars().next() else {
            break;
        };
        idx += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
    }
    idx
}

fn push_plain(spans: &mut Vec<Span<'static>>, text: &str) {
    if !text.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }
}

fn push_styled(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
    }
}

fn highlight_line(line: &str, language: Language, palette: Palette) -> Line<'static> {
    let mut spans = Vec::new();
    let mut idx = 0;

    while idx < line.len() {
        let rest = &line[idx..];
        if language.slash_starts_comment() && rest.starts_with("//") {
            push_styled(&mut spans, rest, style(palette.comment));
            break;
        }
        if language.hash_starts_comment() && rest.starts_with('#') {
            push_styled(&mut spans, rest, style(palette.comment));
            break;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };

        if ch == '"' || ch == '\'' || (ch == '`' && language.allows_backtick_strings()) {
            let end = consume_string(line, idx, ch);
            push_styled(&mut spans, &line[idx..end], style(palette.string));
            idx = end;
            continue;
        }

        if ch.is_ascii_digit() {
            let start = idx;
            idx += ch.len_utf8();
            while idx < line.len() {
                let Some(next) = line[idx..].chars().next() else {
                    break;
                };
                if next.is_ascii_alphanumeric() || matches!(next, '_' | '.') {
                    idx += next.len_utf8();
                } else {
                    break;
                }
            }
            push_styled(&mut spans, &line[start..idx], style(palette.number));
            continue;
        }

        if is_ident_start(ch) {
            let start = idx;
            idx += ch.len_utf8();
            while idx < line.len() {
                let Some(next) = line[idx..].chars().next() else {
                    break;
                };
                if is_ident_continue(next) {
                    idx += next.len_utf8();
                } else {
                    break;
                }
            }
            let token = &line[start..idx];
            if is_keyword(language, token) {
                push_styled(&mut spans, token, bold_style(palette.keyword));
            } else if token
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
            {
                push_styled(&mut spans, token, style(palette.type_name));
            } else {
                push_plain(&mut spans, token);
            }
            continue;
        }

        if ch.is_ascii_punctuation() {
            let end = idx + ch.len_utf8();
            push_styled(&mut spans, &line[idx..end], style(palette.punctuation));
            idx = end;
            continue;
        }

        let end = idx + ch.len_utf8();
        push_plain(&mut spans, &line[idx..end]);
        idx = end;
    }

    if spans.is_empty() {
        Line::raw(String::new())
    } else {
        Line::from(spans)
    }
}

pub(crate) fn highlight_code_to_lines(code: &str, lang: &str) -> Vec<Line<'static>> {
    let line_count = code.lines().count();
    if exceeds_highlight_limits(code.len(), line_count) {
        return code.lines().map(|l| Line::raw(l.to_string())).collect();
    }

    let language = match Language::from_token(lang) {
        Some(language) => language,
        None => return code.lines().map(|l| Line::raw(l.to_string())).collect(),
    };
    let palette = palette();
    code.lines()
        .map(|line| highlight_line(line, language, palette))
        .collect()
}
