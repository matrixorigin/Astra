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

fn truncate_to_width(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        truncate_str(s, max_chars.saturating_sub(1))
    }
}

/// Shorten a path by keeping the filename and truncating the directory prefix with "...".
#[must_use]
pub fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.is_empty() {
        return truncate_to_width(path, max_chars);
    }

    let filename = parts.last().copied().unwrap_or("");
    if filename.chars().count() >= max_chars.saturating_sub(4) {
        return truncate_to_width(filename, max_chars);
    }

    if parts.len() >= 2 {
        let parent = parts[parts.len() - 2];
        let short = format!(".../{parent}/{filename}");
        if short.chars().count() <= max_chars {
            return short;
        }
    }

    format!(".../{filename}")
}

/// Format the most readable GitHub repository display for tool previews.
#[must_use]
pub fn github_repo_display(owner: Option<&str>, repo: Option<&str>) -> Option<String> {
    match (owner, repo) {
        (Some(owner), Some(repo)) if !repo.contains('/') => Some(format!("{owner}/{repo}")),
        (_, Some(repo)) => Some(repo.to_string()),
        _ => None,
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

    #[test]
    fn shorten_path_keeps_filename_when_possible() {
        assert_eq!(shorten_path("short.txt", 20), "short.txt");
        assert_eq!(shorten_path("/a/b/c/d/e/file.txt", 15), ".../e/file.txt");
        assert_eq!(shorten_path("/a/very_long_filename.txt", 10), "very_long…");
        assert_eq!(shorten_path("/a/b/c/short.txt", 14), ".../short.txt");
    }

    #[test]
    fn github_repo_display_prefers_owner_repo_pair() {
        assert_eq!(
            github_repo_display(Some("matrixorigin"), Some("astra")).as_deref(),
            Some("matrixorigin/astra")
        );
        assert_eq!(
            github_repo_display(None, Some("matrixorigin/astra")).as_deref(),
            Some("matrixorigin/astra")
        );
        assert_eq!(github_repo_display(Some("matrixorigin"), None), None);
    }
}
