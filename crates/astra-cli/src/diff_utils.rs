/// Parse a unified diff hunk header `@@ -old_start,old_count +new_start,new_count @@`
/// Returns (old_start-1, new_start-1) — the line *before* the first changed line,
/// so callers can increment before rendering each line.
pub(crate) fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let mut old_start: Option<u32> = None;
    let mut new_start: Option<u32> = None;
    for part in header.split_whitespace() {
        if let Some(s) = part.strip_prefix('-') {
            old_start = s.split(',').next()?.parse().ok();
        } else if let Some(s) = part.strip_prefix('+') {
            new_start = s.split(',').next()?.parse().ok();
        }
    }
    Some((old_start?.saturating_sub(1), new_start?.saturating_sub(1)))
}

#[cfg(test)]
mod tests {
    use super::parse_hunk_header;

    #[test]
    fn parse_hunk_header_returns_pre_change_line_numbers() {
        assert_eq!(parse_hunk_header("@@ -41,2 +99,3 @@"), Some((40, 98)));
    }

    #[test]
    fn parse_hunk_header_rejects_invalid_headers() {
        assert_eq!(parse_hunk_header("@@ malformed @@"), None);
        assert_eq!(parse_hunk_header("@@ -41,2 @@"), None);
        assert_eq!(parse_hunk_header("@@ +99,3 @@"), None);
    }
}
