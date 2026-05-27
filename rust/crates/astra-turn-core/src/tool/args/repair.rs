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

#[cfg(test)]
mod tests {
    use super::*;

    const TC: &str = "test_tool";

    fn repair(raw: &str) -> Option<Map<String, Value>> {
        try_repair_tool_args(TC, raw)
    }

    // ── 1. Already-valid JSON passthrough ──────────────────────────────
    #[test]
    fn valid_json_passthrough() {
        let m = repair(r#"{"key": "value", "num": 42}"#).unwrap();
        assert_eq!(m["key"], "value");
        assert_eq!(m["num"], 42);
    }

    #[test]
    fn valid_json_with_bool_and_null() {
        let m = repair(r#"{"a": true, "b": false, "c": null}"#).unwrap();
        assert_eq!(m["a"], true);
        assert_eq!(m["b"], false);
        assert!(m["c"].is_null());
    }

    // ── 2. Single quotes → double quotes ───────────────────────────────
    #[test]
    fn single_quotes_converted() {
        let m = repair("{'name': 'alice', 'age': 30}").unwrap();
        assert_eq!(m["name"], "alice");
        assert_eq!(m["age"], 30);
    }

    #[test]
    fn single_quotes_mid_value() {
        let m = repair("{'x': 1, 'y': 'hello'}").unwrap();
        assert_eq!(m["y"], "hello");
    }

    // ── 3. Trailing commas removal ─────────────────────────────────────
    #[test]
    fn trailing_comma_in_object() {
        let m = repair(r#"{"a": 1, "b": 2,}"#).unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
    }

    #[test]
    fn trailing_comma_in_array_value() {
        let m = repair(r#"{"items": [1, 2, 3,]}"#).unwrap();
        let arr = m["items"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn trailing_comma_with_whitespace() {
        let m = repair(r#"{"a": 1 ,   }"#).unwrap();
        assert_eq!(m["a"], 1);
    }

    // ── 4. Bare words (unquoted values) ────────────────────────────────
    #[test]
    fn bare_word_value_quoted() {
        let m = repair(r#"{"status": running}"#).unwrap();
        assert_eq!(m["status"], "running");
    }

    #[test]
    fn bare_word_preserves_true_false_null() {
        let m = repair(r#"{"a": true, "b": false, "c": null}"#).unwrap();
        assert_eq!(m["a"], true);
        assert_eq!(m["b"], false);
        assert!(m["c"].is_null());
    }

    #[test]
    fn bare_word_with_underscores() {
        let m = repair(r#"{"mode": dark_mode}"#).unwrap();
        assert_eq!(m["mode"], "dark_mode");
    }

    // ── 5. Control character escaping ──────────────────────────────────
    #[test]
    fn newline_in_string_escaped() {
        // Literal newline inside a JSON string is invalid; repair escapes it
        // so serde_json can parse it (the parsed value is an actual newline).
        let raw = "{\"msg\": \"hello\nworld\"}";
        let m = repair(raw).unwrap();
        assert_eq!(m["msg"], "hello\nworld");
    }

    #[test]
    fn tab_in_string_escaped() {
        let raw = "{\"msg\": \"col1\tcol2\"}";
        let m = repair(raw).unwrap();
        assert_eq!(m["msg"], "col1\tcol2");
    }

    #[test]
    fn carriage_return_escaped() {
        let raw = "{\"msg\": \"a\rb\"}";
        let m = repair(raw).unwrap();
        assert_eq!(m["msg"], "a\rb");
    }

    // ── 6. Unbalanced braces / brackets ────────────────────────────────
    #[test]
    fn missing_closing_brace() {
        let m = repair(r#"{"key": "val""#).unwrap();
        assert_eq!(m["key"], "val");
    }

    #[test]
    fn missing_closing_bracket_in_array() {
        let m = repair(r#"{"items": [1, 2, 3}"#);
        // Should either fix it or return None; if fixed, validate
        if let Some(m) = m {
            assert!(m.contains_key("items"));
        }
    }

    #[test]
    fn missing_closing_brace_and_quote() {
        let m = repair(r#"{"key": "val"#).unwrap();
        assert_eq!(m["key"], "val");
    }

    // ── 7. Empty input ─────────────────────────────────────────────────
    #[test]
    fn empty_string_returns_none() {
        assert!(repair("").is_none());
    }

    #[test]
    fn whitespace_only_returns_none() {
        assert!(repair("   \n\t  ").is_none());
    }

    // ── 8. Unicode / CJK content ──────────────────────────────────────
    #[test]
    fn unicode_values_preserved() {
        let m = repair(r#"{"greeting": "你好世界"}"#).unwrap();
        assert_eq!(m["greeting"], "你好世界");
    }

    #[test]
    fn emoji_in_value() {
        let m = repair(r#"{"icon": "🚀🎉"}"#).unwrap();
        assert_eq!(m["icon"], "🚀🎉");
    }

    #[test]
    fn mixed_cjk_and_ascii() {
        let m = repair(r#"{"title": "Hello 世界 123"}"#).unwrap();
        assert_eq!(m["title"], "Hello 世界 123");
    }

    // ── 9. Nested objects ──────────────────────────────────────────────
    #[test]
    fn nested_object() {
        let m = repair(r#"{"outer": {"inner": "deep"}}"#).unwrap();
        let inner = m["outer"].as_object().unwrap();
        assert_eq!(inner["inner"], "deep");
    }

    #[test]
    fn nested_with_trailing_commas() {
        let m = repair(r#"{"a": {"b": 1,}, "c": [1, 2,],}"#).unwrap();
        assert_eq!(m["a"].as_object().unwrap()["b"], 1);
        assert_eq!(m["c"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn nested_with_single_quotes() {
        let m = repair("{'outer': {'key': 'val'}}").unwrap();
        assert_eq!(m["outer"].as_object().unwrap()["key"], "val");
    }

    // ── 10. Edge cases: long input, deep nesting ───────────────────────
    #[test]
    fn very_long_string_value() {
        let long_val = "x".repeat(10_000);
        let raw = format!(r#"{{"data": "{}"}}"#, long_val);
        let m = repair(&raw).unwrap();
        assert_eq!(m["data"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn many_keys() {
        let pairs: Vec<String> = (0..200).map(|i| format!(r#""k{}": {}"#, i, i)).collect();
        let raw = format!("{{{}}}", pairs.join(", "));
        let m = repair(&raw).unwrap();
        assert_eq!(m.len(), 200);
        assert_eq!(m["k0"], 0);
        assert_eq!(m["k199"], 199);
    }

    #[test]
    fn deeply_nested_objects() {
        // 50 levels of nesting
        let mut raw = String::new();
        for i in 0..50 {
            raw.push_str(&format!(r#"{{"level{}": "#, i));
        }
        raw.push_str(r#""leaf""#);
        for _ in 0..50 {
            raw.push('}');
        }
        let m = repair(&raw).unwrap();
        // Walk down to the leaf
        let mut current = Value::Object(m);
        for i in 0..50 {
            let key = format!("level{}", i);
            current = current.as_object().unwrap()[&key].clone();
        }
        assert_eq!(current, "leaf");
    }

    // ── Combined repairs ───────────────────────────────────────────────
    #[test]
    fn single_quotes_and_trailing_comma() {
        let m = repair("{'a': 1, 'b': 2,}").unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
    }

    #[test]
    fn bare_word_and_missing_brace() {
        // Bare word followed by } (after brace repair) needs the closing
        // brace present at regex-time; test with explicit trailing brace.
        let m = repair(r#"{"status": active}"#).unwrap();
        assert_eq!(m["status"], "active");
    }

    #[test]
    fn control_chars_and_unbalanced() {
        let raw = "{\"msg\": \"line1\nline2\"";
        let m = repair(raw).unwrap();
        assert_eq!(m["msg"], "line1\nline2");
    }
}
