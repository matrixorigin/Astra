//! Memoria insights digest.
//!
//! Compact prompt fragment rendered from Memoria recall hits so the agent
//! sees "what it previously learned" in its own system prompt.
//!
//! Kept as a pure function to be unit-testable and reused by both CLI
//! `agentic_loop_turn.rs` (producer) and runtime `bridge_inprocess.rs`
//! (consumer) without circular deps.

const MAX_BULLETS: usize = 4;
const MAX_CHARS_PER_BULLET: usize = 180;
const MIN_CONTENT_LEN: usize = 8;
const SECTION_HEADER: &str = "## Memoria Recall";

/// Render a bulleted digest from raw memory hit contents.
///
/// Returns `None` when there is nothing substantive to surface (empty
/// input, all hits too short, or every bullet got squashed to whitespace).
/// The caller is expected to inject the returned text verbatim into the
/// system prompt (usually prefixed by a blank line).
pub fn render_digest(contents: &[String]) -> Option<String> {
    let mut bullets: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw in contents {
        if bullets.len() >= MAX_BULLETS {
            break;
        }
        let cleaned = compact_one_line(raw);
        if cleaned.len() < MIN_CONTENT_LEN {
            continue;
        }
        let key = cleaned.to_lowercase();
        if !seen.insert(key) {
            continue;
        }
        // Decode business type prefix for categorized display.
        let (cat, body) = astra_prompts::memory_types::decode(&cleaned);
        let label = match cat {
            Some(c) => format!("[{}] ", c.content_prefix().trim_matches(|ch| ch == '[' || ch == ']')),
            None => String::new(),
        };
        bullets.push(format!(
            "{label}{}",
            truncate_with_ellipsis(body, MAX_CHARS_PER_BULLET)
        ));
    }
    if bullets.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(SECTION_HEADER.len() + bullets.len() * 64);
    out.push_str(SECTION_HEADER);
    out.push('\n');
    for b in bullets {
        out.push_str("- ");
        out.push_str(&b);
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

fn compact_one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_none() {
        assert!(render_digest(&[]).is_none());
    }

    #[test]
    fn short_hits_filtered_out() {
        let hits = vec!["hi".to_string(), "ok".to_string()];
        assert!(render_digest(&hits).is_none());
    }

    #[test]
    fn renders_header_and_bullets() {
        let hits = vec![
            "User prefers Rust for CLI work.".to_string(),
            "Astra should always run tests before committing.".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(out.starts_with("## Memoria Recall"));
        assert!(out.contains("- User prefers Rust"));
        assert!(out.contains("- Astra should always run tests"));
    }

    #[test]
    fn deduplicates_identical_content() {
        let hits = vec![
            "Always run `cargo test` before commit.".to_string(),
            "Always run `cargo test` before commit.".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert_eq!(out.matches("Always run").count(), 1);
    }

    #[test]
    fn truncates_long_hit_with_ellipsis() {
        let long = "x".repeat(500);
        let out = render_digest(&[long]).expect("digest");
        assert!(out.contains('…'));
        let bullet_len = out
            .lines()
            .find(|l| l.starts_with("- "))
            .map(|l| l.chars().count())
            .unwrap_or(0);
        // "- " + MAX_CHARS_PER_BULLET chars max
        assert!(bullet_len <= 2 + MAX_CHARS_PER_BULLET);
    }

    #[test]
    fn caps_bullet_count() {
        let hits: Vec<String> = (0..10)
            .map(|i| format!("Insight number {i} with enough text to pass filter."))
            .collect();
        let out = render_digest(&hits).expect("digest");
        let n = out.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(n, MAX_BULLETS);
    }

    #[test]
    fn collapses_whitespace() {
        let hits = vec!["multi\n  line\n\t\tcontent with gaps".to_string()];
        let out = render_digest(&hits).expect("digest");
        assert!(out.contains("- multi line content with gaps"));
    }

    #[test]
    fn categorized_rendering_with_business_prefix() {
        let hits = vec![
            "[feedback] always use RS256 for JWT signing".to_string(),
            "[user] senior Rust engineer, prefers CLI tools".to_string(),
            "plain legacy memory without prefix".to_string(),
        ];
        let out = render_digest(&hits).expect("digest");
        assert!(
            out.contains("[feedback] always use RS256"),
            "should show category label for typed memories"
        );
        assert!(
            out.contains("[user] senior Rust"),
            "should show category label for user memories"
        );
        assert!(
            out.contains("- plain legacy memory"),
            "legacy memories should render without label"
        );
    }

    #[test]
    fn categorized_rendering_strips_prefix_from_body() {
        let hits = vec!["[project] merge freeze starts May 8th for mobile release".to_string()];
        let out = render_digest(&hits).expect("digest");
        assert!(
            !out.contains("[project] [project]"),
            "should not double the prefix"
        );
        assert!(out.contains("[project] merge freeze"));
    }
}
