use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};

pub(crate) fn terminal_hyperlinks_enabled() -> bool {
    std::env::var("ASTRA_OSC8")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "off" | "no"))
        .unwrap_or(true)
}

pub(crate) fn sanitize_osc8_component(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(*c, '\x1b' | '\x07') && !c.is_control())
        .collect()
}

pub(crate) fn osc8_link(uri: &str, label: &str) -> String {
    let uri = sanitize_osc8_component(uri);
    let label = sanitize_osc8_component(label);
    format!("\x1b]8;;{uri}\x1b\\{label}\x1b]8;;\x1b\\")
}

pub(crate) fn hyperlink_line_file_paths(line: &Line<'static>, cwd: Option<&Path>) -> Line<'static> {
    if !terminal_hyperlinks_enabled() {
        return line.clone();
    }

    let mut changed = false;
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|span| {
            let content = hyperlink_text_file_paths(span.content.as_ref(), cwd);
            if content != span.content.as_ref() {
                changed = true;
            }
            Span::styled(content, span.style)
        })
        .collect();

    if !changed {
        return line.clone();
    }

    let mut out = Line::from(spans);
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

fn hyperlink_text_file_paths(text: &str, cwd: Option<&Path>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(ch) = text[cursor..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            out.push(ch);
            cursor += ch.len_utf8();
            continue;
        }

        let start = cursor;
        cursor += ch.len_utf8();
        while cursor < text.len() {
            let Some(next) = text[cursor..].chars().next() else {
                break;
            };
            if next.is_whitespace() {
                break;
            }
            cursor += next.len_utf8();
        }

        let token = &text[start..cursor];
        if let Some(linked) = hyperlink_file_path_token(token, cwd) {
            out.push_str(&linked);
            changed = true;
        } else {
            out.push_str(token);
        }
    }

    if changed { out } else { text.to_string() }
}

fn hyperlink_file_path_token(raw_token: &str, cwd: Option<&Path>) -> Option<String> {
    if raw_token.contains("\x1b]8;;") {
        return None;
    }

    let (prefix, core, suffix) = split_surrounding_punctuation(raw_token);
    if core.is_empty() || looks_like_url(core) {
        return None;
    }

    let (path, _) = split_optional_line_suffix(core);
    if !is_file_path_like(path) {
        return None;
    }

    let uri = file_uri_for_path(path, cwd)?;
    Some(format!("{prefix}{}{suffix}", osc8_link(&uri, core)))
}

fn split_surrounding_punctuation(token: &str) -> (&str, &str, &str) {
    let start = token
        .char_indices()
        .find(|(_, c)| !is_leading_trimmed_token_punctuation(*c))
        .map(|(idx, _)| idx)
        .unwrap_or(token.len());
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| !is_trailing_trimmed_token_punctuation(*c))
        .map(|(idx, c)| idx + c.len_utf8())
        .unwrap_or(start);
    if start >= end {
        return (token, "", "");
    }
    (&token[..start], &token[start..end], &token[end..])
}

fn is_leading_trimmed_token_punctuation(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '<' | '\'' | '"')
}

fn is_trailing_trimmed_token_punctuation(c: char) -> bool {
    matches!(
        c,
        ')' | ']' | '}' | '>' | ',' | '.' | ';' | ':' | '!' | '\'' | '"'
    )
}

fn looks_like_url(token: &str) -> bool {
    token.contains("://")
        || token.starts_with("www.")
        || token.starts_with("localhost:")
        || token.starts_with("localhost/")
}

fn split_optional_line_suffix(token: &str) -> (&str, Option<&str>) {
    let Some((path, suffix)) = token.rsplit_once(':') else {
        return (token, None);
    };
    if suffix.chars().all(|c| c.is_ascii_digit()) && path.contains('/') {
        (path, Some(suffix))
    } else {
        (token, None)
    }
}

fn is_file_path_like(path: &str) -> bool {
    is_absolute_path_like(path) || is_relative_path_like(path)
}

fn is_absolute_path_like(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && path
            .chars()
            .all(|c| !c.is_control() && !matches!(c, '\x1b' | '\x07'))
}

fn is_relative_path_like(path: &str) -> bool {
    if !(path.starts_with("./") || path.starts_with("../") || path.contains('/')) {
        return false;
    }
    if path.ends_with('/') || path.contains("://") {
        return false;
    }
    let Some(last) = path.rsplit('/').next() else {
        return false;
    };
    last.contains('.')
        && !last.starts_with('.')
        && path
            .chars()
            .all(|c| !c.is_control() && !matches!(c, '\x1b' | '\x07' | '*' | '?'))
}

fn file_uri_for_path(path: &str, cwd: Option<&Path>) -> Option<String> {
    let clean = sanitize_osc8_component(path);
    if clean.is_empty() {
        return None;
    }

    let candidate = if clean.starts_with('/') {
        PathBuf::from(&clean)
    } else if let Some(cwd) = cwd {
        cwd.join(&clean)
    } else {
        return Some(format!("file://./{clean}"));
    };

    url::Url::from_file_path(candidate)
        .ok()
        .map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{hyperlink_text_file_paths, split_surrounding_punctuation};

    #[test]
    fn split_surrounding_punctuation_handles_empty_core() {
        let (prefix, core, suffix) = split_surrounding_punctuation(r#"<<")"#);
        assert_eq!(prefix, r#"<<")"#);
        assert_eq!(core, "");
        assert_eq!(suffix, "");
    }

    #[test]
    fn hyperlink_text_file_paths_ignores_punctuation_only_tokens() {
        let text = r#"prefix <<") suffix"#;
        assert_eq!(hyperlink_text_file_paths(text, None), text);
    }
}
