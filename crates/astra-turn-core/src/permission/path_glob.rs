//! Issue #326 P5 / R2 Major 2: path glob matcher for permission rules.
//!
//! Supports the gitignore-style patterns that plan v3 §P5 promises:
//!
//! - `*`         — matches any sequence within a single path segment
//! - `**`        — matches any number of segments (including zero)
//! - `?`         — matches a single character
//! - `{a,b,c}`   — matches any of the listed alternatives
//! - everything else is matched literally
//!
//! We deliberately avoid pulling in `globset` (an extra ~2k LoC of
//! transitive deps) because:
//!
//! 1. The permission rule grammar is small — a few hundred globs at
//!    most across user + project files. Performance isn't the issue.
//! 2. We need *exact* compatibility with the permission rule
//!    grammar's quoting rules, so a hand-rolled matcher avoids the
//!    impedance-mismatch of "what does globset escape vs what does
//!    permission_rule_grammar escape".
//! 3. The glob alphabet here is intentionally a subset of gitignore;
//!    we want it to feel familiar to users without inheriting
//!    gitignore's negation/anchor surprises.
//!
//! ## Matching rules summary
//!
//! - `**` followed by `/` matches any number of complete segments
//!   (so `src/**/*.rs` matches `src/lib.rs` and `src/auth/login.rs`).
//! - Other `**` forms match any characters, including `/`; command rules
//!   use this to match paths within the whole command string.
//! - `*` does NOT cross `/` (so `src/*.rs` does NOT match
//!   `src/auth/login.rs`).
//! - The matcher is anchored at both ends. To match a suffix, use
//!   a leading `**/`.

use std::collections::HashSet;

/// Match `path` against `pattern` using the rules described in the
/// module docs.
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p_chars: Vec<char> = pattern.chars().collect();
    let s_chars: Vec<char> = path.chars().collect();
    glob_match_chars(&p_chars, 0, &s_chars, 0)
}

fn glob_match_chars(p: &[char], pi: usize, s: &[char], si: usize) -> bool {
    let mut pi = pi;
    let mut si = si;
    while pi < p.len() {
        match p[pi] {
            '*' if pi + 1 < p.len() && p[pi + 1] == '*' => {
                let after_doublestar = pi + 2;
                let whole_segment = pi == 0 || p[pi - 1] == '/' || p[pi - 1].is_whitespace();
                if after_doublestar == p.len() {
                    return true;
                }
                if whole_segment && p.get(after_doublestar) == Some(&'/') {
                    // A recursive segment may consume zero or more complete
                    // path components, never a prefix of the next component.
                    let next = after_doublestar + 1;
                    if glob_match_chars(p, next, s, si) {
                        return true;
                    }
                    for k in si..s.len() {
                        if s[k] == '/' && glob_match_chars(p, next, s, k + 1) {
                            return true;
                        }
                    }
                } else {
                    // Non-segment command globs retain their existing
                    // cross-separator semantics.
                    let next = if p.get(after_doublestar) == Some(&'/') {
                        after_doublestar + 1
                    } else {
                        after_doublestar
                    };
                    let mut k = si;
                    loop {
                        if glob_match_chars(p, next, s, k) {
                            return true;
                        }
                        if k == s.len() {
                            break;
                        }
                        k += 1;
                    }
                }
                return false;
            }
            '*' => {
                // Single * — match within a segment (no `/`).
                let next_pi = pi + 1;
                // try matching zero, one, two, … chars but stop at `/`
                if glob_match_chars(p, next_pi, s, si) {
                    return true;
                }
                let mut k = si;
                while k < s.len() && s[k] != '/' {
                    k += 1;
                    if glob_match_chars(p, next_pi, s, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if si >= s.len() || s[si] == '/' {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            '{' => {
                // Brace alternative.
                let close = match find_matching_brace(p, pi) {
                    Some(k) => k,
                    None => return false,
                };
                let alts = split_brace_alternatives(&p[pi + 1..close]);
                let after_brace = close + 1;
                for alt in alts {
                    let mut composed: Vec<char> = alt.to_vec();
                    composed.extend_from_slice(&p[after_brace..]);
                    if glob_match_chars(&composed, 0, s, si) {
                        return true;
                    }
                }
                return false;
            }
            c => {
                if si >= s.len() || s[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }
    si == s.len()
}

fn find_matching_brace(p: &[char], open_idx: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = open_idx + 1;
    while i < p.len() {
        match p[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_brace_alternatives(inner: &[char]) -> Vec<&[char]> {
    let mut alts = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    for (i, c) in inner.iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                alts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    alts.push(&inner[start..]);
    alts
}

/// Convenience: check whether any of `patterns` matches `path`.
#[must_use]
pub fn any_glob_matches<S: AsRef<str>>(patterns: impl IntoIterator<Item = S>, path: &str) -> bool {
    patterns.into_iter().any(|p| glob_match(p.as_ref(), path))
}

/// Quickly classify a path against a set of "sensitive" globs.
/// Used by the engine's SensitivePath step (see plan v3 §P2).
#[must_use]
pub fn matches_any(path: &str, patterns: &HashSet<String>) -> bool {
    patterns.iter().any(|p| glob_match(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Single-star (within one segment) ──────────────────────────

    #[test]
    fn star_matches_within_segment() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/auth/login.rs"));
    }

    #[test]
    fn star_does_not_match_path_separator() {
        assert!(!glob_match("*.rs", "src/lib.rs"));
    }

    #[test]
    fn star_zero_chars() {
        assert!(glob_match("foo*", "foo"));
        assert!(glob_match("*foo", "foo"));
    }

    // ── Double-star ──────────────────────────────────────────────

    #[test]
    fn doublestar_matches_zero_or_more_segments() {
        assert!(glob_match("src/**/*.rs", "src/lib.rs"));
        assert!(glob_match("src/**/*.rs", "src/auth/login.rs"));
        assert!(glob_match("src/**/*.rs", "src/auth/oauth/google.rs"));
    }

    #[test]
    fn doublestar_match_anywhere() {
        assert!(glob_match("**/test_*.rs", "test_foo.rs"));
        assert!(glob_match("**/test_*.rs", "src/auth/test_foo.rs"));
        assert!(glob_match("**/.env*", ".env"));
        assert!(glob_match("**/.env*", "config/.env.production"));
        assert!(!glob_match("**/.env*", "service.envoy"));
        assert!(!glob_match(
            "**/.aws/credentials",
            "service.aws/credentials"
        ));
        assert!(!glob_match(
            "cat **/.aws/credentials",
            "cat service.aws/credentials"
        ));
        assert!(glob_match(
            "**/.aws/credentials",
            "service/.aws/credentials"
        ));
        assert!(glob_match("cat **", "cat dir/file"));
        assert!(glob_match("foo**bar", "foo/other/bar"));
    }

    #[test]
    fn doublestar_matches_empty() {
        // src/**/lib.rs should match src/lib.rs (zero
        // intermediate segments).
        assert!(glob_match("src/**/lib.rs", "src/lib.rs"));
    }

    // ── Question mark ────────────────────────────────────────────

    #[test]
    fn question_matches_one_char() {
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(!glob_match("file?.txt", "file12.txt"));
    }

    #[test]
    fn question_does_not_match_separator() {
        assert!(!glob_match("file?txt", "file/txt"));
    }

    // ── Brace alternatives ───────────────────────────────────────

    #[test]
    fn brace_alternatives() {
        assert!(glob_match("*.{rs,ts,js}", "lib.rs"));
        assert!(glob_match("*.{rs,ts,js}", "lib.ts"));
        assert!(glob_match("*.{rs,ts,js}", "lib.js"));
        assert!(!glob_match("*.{rs,ts,js}", "lib.py"));
    }

    #[test]
    fn brace_with_glob() {
        assert!(glob_match("src/{auth,admin}/*.rs", "src/auth/login.rs"));
        assert!(glob_match("src/{auth,admin}/*.rs", "src/admin/users.rs"));
        assert!(!glob_match(
            "src/{auth,admin}/*.rs",
            "src/billing/charge.rs"
        ));
    }

    // ── Sensitive path patterns from scenario #8 ─────────────────

    #[test]
    fn sensitive_dotenv_pattern() {
        assert!(glob_match("**/.env*", ".env"));
        assert!(glob_match("**/.env*", ".env.local"));
        assert!(glob_match("**/.env*", "secrets/.env.production"));
        assert!(!glob_match("**/.env*", "envvars.json"));
    }

    #[test]
    fn sensitive_pem_pattern() {
        assert!(glob_match("**/*.pem", "cert.pem"));
        assert!(glob_match("**/*.pem", "ssl/server.pem"));
        assert!(glob_match("**/*.pem", "etc/ssl/private/server.pem"));
    }

    #[test]
    fn sensitive_ssh_dir_pattern() {
        assert!(glob_match("**/.ssh/**", ".ssh/id_rsa"));
        assert!(glob_match("**/.ssh/**", "home/user/.ssh/id_rsa"));
        assert!(glob_match("**/.ssh/**", "home/user/.ssh/known_hosts"));
    }

    // ── Edge cases ───────────────────────────────────────────────

    #[test]
    fn anchored_at_both_ends() {
        // No leading wildcard → only matches strings that start
        // with the literal prefix.
        assert!(!glob_match("lib.rs", "src/lib.rs"));
    }

    #[test]
    fn empty_pattern_matches_only_empty_string() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "anything"));
    }

    #[test]
    fn literal_chars_match_exactly() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("cargo.toml", "Cargo.toml"));
    }

    #[test]
    fn any_glob_matches_helper() {
        assert!(any_glob_matches(["*.rs", "*.ts"], "lib.rs"));
        assert!(any_glob_matches(["*.rs", "*.ts"], "ui.ts"));
        assert!(!any_glob_matches(["*.rs", "*.ts"], "config.toml"));
    }
}
