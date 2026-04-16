//! Conditional skill activation — path-glob matching and trigger detection.
//!
//! Skills with `paths` frontmatter are only visible after the model touches
//! matching files (conditional activation based on path globs).

use super::manifest::SkillManifest;

/// Check if a file path matches any of the skill's activation path globs.
///
/// Uses a simplified glob matching that supports `*` (single segment) and
/// `**` (any number of segments).
pub fn path_matches_skill(file_path: &str, skill: &SkillManifest) -> bool {
    if skill.paths.is_empty() {
        return false;
    }
    skill
        .paths
        .iter()
        .any(|pattern| glob_match(pattern, file_path))
}

/// Simplified glob matching supporting `*` and `**`.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    glob_match_parts(&pattern_parts, &path_parts)
}

fn glob_match_parts(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        // ** matches zero or more path segments
        let rest_pattern = &pattern[1..];
        for i in 0..=path.len() {
            if glob_match_parts(rest_pattern, &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if segment_matches(pattern[0], path[0]) {
        glob_match_parts(&pattern[1..], &path[1..])
    } else {
        false
    }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Support *.ext patterns
    if let Some(suffix) = pattern.strip_prefix('*') {
        return segment.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return segment.starts_with(prefix);
    }
    pattern == segment
}

/// Tracks which conditional skills have been activated.
#[derive(Debug, Default)]
pub struct ConditionalSkillTracker {
    /// Skill names that have been activated by path matches.
    activated: std::collections::HashSet<String>,
    /// File paths that have been seen.
    seen_paths: std::collections::HashSet<String>,
}

impl ConditionalSkillTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a file path and check if it activates any conditional skills.
    /// Returns the names of newly activated skills.
    pub fn record_path(
        &mut self,
        file_path: &str,
        conditional_skills: &[SkillManifest],
    ) -> Vec<String> {
        if self.seen_paths.contains(file_path) {
            return Vec::new();
        }
        self.seen_paths.insert(file_path.to_string());

        let mut newly_activated = Vec::new();
        for skill in conditional_skills {
            if !self.activated.contains(&skill.name) && path_matches_skill(file_path, skill) {
                self.activated.insert(skill.name.clone());
                newly_activated.push(skill.name.clone());
            }
        }
        newly_activated
    }

    /// Check if a skill has been activated.
    pub fn is_activated(&self, name: &str) -> bool {
        self.activated.contains(name)
    }

    /// Get all activated skill names.
    pub fn activated_skills(&self) -> Vec<&str> {
        self.activated.iter().map(|s| s.as_str()).collect()
    }

    /// Reset all activation state (used on registry refresh).
    pub fn reset(&mut self) {
        self.activated.clear();
        self.seen_paths.clear();
    }
}

// ── Trigger detection ────────────────────────────────────────────────────────

/// Detect which skills are triggered by keywords in a message.
///
/// Performs word-level matching — triggers must appear as whole words
/// (case-insensitive). Returns skill names sorted by trigger specificity
/// (longer triggers first).
pub fn detect_triggers(skills: &[SkillManifest], message: &str) -> Vec<String> {
    let message_lower = message.to_lowercase();
    let words: Vec<&str> = message_lower.split_whitespace().collect();

    let mut matches: Vec<(String, usize)> = Vec::new();

    for skill in skills {
        for trigger in &skill.triggers {
            let trigger_lower = trigger.to_lowercase();
            if words.contains(&trigger_lower.as_str())
                || is_word_boundary_match(&message_lower, &trigger_lower)
            {
                matches.push((skill.name.clone(), trigger.len()));
                break;
            }
        }
    }

    matches.sort_by_key(|m| std::cmp::Reverse(m.1));
    matches.into_iter().map(|(name, _)| name).collect()
}

fn is_word_boundary_match(text: &str, pattern: &str) -> bool {
    if pattern.chars().any(is_cjk_char) {
        return text.contains(pattern);
    }

    let mut search_start = 0;
    while search_start < text.len() {
        let search_slice = &text[search_start..];
        let Some(pos) = search_slice.find(pattern) else {
            break;
        };

        let abs_pos = search_start + pos;
        let end_pos = abs_pos + pattern.len();

        let start_ok = abs_pos == 0 || {
            let prev_slice = &text[..abs_pos];
            prev_slice
                .chars()
                .last()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        };

        let end_ok = end_pos >= text.len() || {
            let next_slice = &text[end_pos..];
            next_slice
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        };

        if start_ok && end_ok {
            return true;
        }

        search_start = abs_pos + text[abs_pos..].chars().next().map_or(1, |c| c.len_utf8());
    }
    false
}

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |
        '\u{3400}'..='\u{4DBF}' |
        '\u{20000}'..='\u{2A6DF}' |
        '\u{F900}'..='\u{FAFF}' |
        '\u{3000}'..='\u{303F}' |
        '\u{3040}'..='\u{309F}' |
        '\u{30A0}'..='\u{30FF}' |
        '\u{AC00}'..='\u{D7AF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(name: &str, paths: Vec<&str>) -> SkillManifest {
        SkillManifest {
            name: name.into(),
            paths: paths.into_iter().map(|s| s.into()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn glob_match_exact_file() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_match_star() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/main.py"));
        assert!(!glob_match("src/*.rs", "tests/main.rs"));
    }

    #[test]
    fn glob_match_doublestar() {
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(glob_match("src/**/*.rs", "src/sub/deep/file.rs"));
        assert!(!glob_match("src/**/*.rs", "tests/file.rs"));
    }

    #[test]
    fn glob_match_doublestar_prefix() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "a/b/c/d.rs"));
        assert!(!glob_match("**/*.rs", "file.py"));
    }

    #[test]
    fn path_matches_conditional_skill() {
        let skill = make_skill("rust-review", vec!["src/**/*.rs", "tests/**/*.rs"]);
        assert!(path_matches_skill("src/main.rs", &skill));
        assert!(path_matches_skill("tests/unit/foo.rs", &skill));
        assert!(!path_matches_skill("docs/readme.md", &skill));
    }

    #[test]
    fn unconditional_skill_never_matches() {
        let skill = make_skill("basic", vec![]);
        assert!(!path_matches_skill("anything.rs", &skill));
    }

    #[test]
    fn tracker_activates_skills() {
        let skills = vec![
            make_skill("rust-lint", vec!["src/**/*.rs"]),
            make_skill("docs-check", vec!["docs/**/*.md"]),
        ];

        let mut tracker = ConditionalSkillTracker::new();

        let activated = tracker.record_path("src/lib.rs", &skills);
        assert_eq!(activated, vec!["rust-lint"]);
        assert!(tracker.is_activated("rust-lint"));
        assert!(!tracker.is_activated("docs-check"));

        // Same path again — no new activations
        let activated = tracker.record_path("src/lib.rs", &skills);
        assert!(activated.is_empty());

        // Different path activates docs skill
        let activated = tracker.record_path("docs/guide.md", &skills);
        assert_eq!(activated, vec!["docs-check"]);
    }

    #[test]
    fn trigger_detection_word_match() {
        let skills = vec![SkillManifest {
            name: "review".into(),
            triggers: vec!["review".into(), "code-review".into()],
            ..Default::default()
        }];

        let matches = detect_triggers(&skills, "please review this PR");
        assert_eq!(matches, vec!["review"]);

        let matches = detect_triggers(&skills, "previewing the code");
        assert!(matches.is_empty());
    }

    #[test]
    fn trigger_detection_case_insensitive() {
        let skills = vec![SkillManifest {
            name: "debug".into(),
            triggers: vec!["debug".into()],
            ..Default::default()
        }];

        let matches = detect_triggers(&skills, "DEBUG this issue");
        assert_eq!(matches, vec!["debug"]);
    }

    #[test]
    fn trigger_detection_cjk() {
        let skills = vec![SkillManifest {
            name: "review-cn".into(),
            triggers: vec!["审查".into()],
            ..Default::default()
        }];

        let matches = detect_triggers(&skills, "审查一下代码");
        assert_eq!(matches, vec!["review-cn"]);
    }

    #[test]
    fn tracker_reset_clears_all_state() {
        let mut tracker = ConditionalSkillTracker::new();
        let skills = vec![SkillManifest {
            name: "rs-skill".into(),
            paths: vec!["*.rs".into()],
            ..Default::default()
        }];

        let activated = tracker.record_path("main.rs", &skills);
        assert_eq!(activated, vec!["rs-skill"]);
        assert!(tracker.is_activated("rs-skill"));

        tracker.reset();
        assert!(!tracker.is_activated("rs-skill"));
        assert!(tracker.activated_skills().is_empty());

        // After reset, same path can re-activate
        let activated = tracker.record_path("main.rs", &skills);
        assert_eq!(activated, vec!["rs-skill"]);
    }
}
