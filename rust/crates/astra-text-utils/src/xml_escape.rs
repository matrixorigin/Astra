use std::borrow::Cow;

/// Escape XML metacharacters for **attribute values** — `&`, `<`, `>`, `"`, `'`.
///
/// Use this when emitting `<tag name="{value}"/>` shapes; the value can contain
/// either single or double quotes safely.
pub fn xml_escape_attr(input: &str) -> Cow<'_, str> {
    xml_escape(input, /* include_quotes = */ true)
}

/// Escape XML metacharacters for **element text content** — `&`, `<`, `>`.
///
/// Use this when emitting `<tag>{value}</tag>` shapes. Quotes are intentionally
/// left as-is — they are valid in element text. **Do not use this for attribute
/// values**: a `"` in `name="..."` would break out of the attribute. Use
/// [`xml_escape_attr`] there.
///
/// Zero-alloc fast path: returns the borrowed input unchanged when no escape
/// is needed (the common case for tool/skill descriptions).
pub fn xml_escape_text(input: &str) -> Cow<'_, str> {
    xml_escape(input, /* include_quotes = */ false)
}

fn xml_escape(input: &str, include_quotes: bool) -> Cow<'_, str> {
    let needs_escape = input.bytes().any(|byte| {
        matches!(byte, b'&' | b'<' | b'>') || (include_quotes && matches!(byte, b'"' | b'\''))
    });
    if !needs_escape {
        return Cow::Borrowed(input);
    }

    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' if include_quotes => escaped.push_str("&quot;"),
            '\'' if include_quotes => escaped.push_str("&apos;"),
            ch => escaped.push(ch),
        }
    }
    Cow::Owned(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_attr_escapes_attribute_metacharacters() {
        assert_eq!(
            xml_escape_attr(r#"a&b<c>d"e'f"#),
            r#"a&amp;b&lt;c&gt;d&quot;e&apos;f"#
        );
    }

    #[test]
    fn xml_escape_text_leaves_quotes_alone() {
        assert_eq!(
            xml_escape_text(r#"a&b<c>d"e'f"#),
            r#"a&amp;b&lt;c&gt;d"e'f"#
        );
    }

    #[test]
    fn xml_escape_text_zero_alloc_fast_path() {
        let input = "no metacharacters here";
        let escaped = xml_escape_text(input);
        assert!(matches!(escaped, Cow::Borrowed(_)));
    }
}
