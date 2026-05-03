pub(crate) fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(crate) fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    &s[..floor_char_boundary(s, max_bytes)]
}

pub(crate) fn truncate_with_suffix(s: &str, max_bytes: usize, suffix: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let prefix_budget = max_bytes.saturating_sub(suffix.len());
    format!("{}{}", safe_prefix(s, prefix_budget), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_prefix_never_splits_multibyte() {
        let text = "你好".repeat(10);
        for max in 0..text.len() {
            let prefix = safe_prefix(&text, max);
            assert!(text.starts_with(prefix));
            assert!(prefix.is_char_boundary(prefix.len()));
            assert!(prefix.len() <= max);
        }
    }

    #[test]
    fn truncate_with_suffix_respects_byte_budget_for_chinese() {
        let text = "中文".repeat(1000);
        let truncated = truncate_with_suffix(&text, 2000, "…");
        assert!(truncated.len() <= 2000);
        assert!(truncated.ends_with('…'));
    }
}
