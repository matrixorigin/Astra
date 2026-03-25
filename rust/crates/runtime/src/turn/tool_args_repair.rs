use regex::{Captures, Regex};
use serde_json::{Map, Value};

pub fn try_repair_tool_args(tc_name: &str, raw: &str) -> Option<Map<String, Value>> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }

    if s.starts_with("{'") || s.contains(", '") {
        s = s.replace('\'', "\"");
    }

    let trailing_commas = Regex::new(r",\s*([}\]])").expect("trailing comma regex should compile");
    s = trailing_commas.replace_all(&s, "$1").into_owned();

    let (fixed, in_str) = escape_control_chars(&s);
    s = fixed;

    let bare_word_values =
        Regex::new(r#":\s*([a-zA-Z_]\w*)(\s*[,}\]])"#).expect("bare word regex should compile");
    s = bare_word_values
        .replace_all(&s, |captures: &Captures| {
            let word = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            if matches!(word, "true" | "false" | "null") {
                captures
                    .get(0)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default()
            } else {
                format!(
                    r#": "{}"{}"#,
                    word,
                    captures.get(2).map(|m| m.as_str()).unwrap_or_default()
                )
            }
        })
        .into_owned();

    if let Some(parsed) = parse_object(&s) {
        return Some(parsed);
    }

    let depth_brace = s.matches('{').count() as isize - s.matches('}').count() as isize;
    let depth_bracket = s.matches('[').count() as isize - s.matches(']').count() as isize;
    if in_str {
        s.push('"');
    }
    s.push_str(&"]".repeat(depth_bracket.max(0) as usize));
    s.push_str(&"}".repeat(depth_brace.max(0) as usize));

    let _ = tc_name;
    parse_object(&s)
}

fn escape_control_chars(input: &str) -> (String, bool) {
    let chars = input.chars().collect::<Vec<_>>();
    let mut fixed = String::with_capacity(input.len());
    let mut in_str = false;

    for (index, ch) in chars.iter().enumerate() {
        if *ch == '"' && (index == 0 || chars[index - 1] != '\\') {
            in_str = !in_str;
            fixed.push(*ch);
        } else if in_str && *ch == '\n' {
            fixed.push_str("\\n");
        } else if in_str && *ch == '\t' {
            fixed.push_str("\\t");
        } else if in_str && *ch == '\r' {
            fixed.push_str("\\r");
        } else {
            fixed.push(*ch);
        }
    }

    (fixed, in_str)
}

fn parse_object(input: &str) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|value| value.as_object().cloned())
}
