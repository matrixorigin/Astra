//! Unicode-scalar–safe string previews for logs, CLI, and tool summaries.

/// First `n` Unicode scalar values (no ellipsis). For display prefixes (session id, etc.).
#[must_use]
pub fn prefix_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[must_use]
pub fn truncate_str(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_none() {
        s.to_string()
    } else {
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello…");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate_str("abc", 3), "abc");
    }

    #[test]
    fn truncate_str_respects_utf8_scalars() {
        let s = "数据—flow";
        assert_eq!(truncate_str(s, 3), "数据—…");
    }

    #[test]
    fn prefix_chars_respects_utf8_scalars() {
        assert_eq!(prefix_chars("数据—flow", 3), "数据—");
    }
}
