//! Semantic classification for process exit codes.
//!
//! POSIX exit codes are not a boolean success/failure API. Tools like
//! `grep`, `diff`, and `test` intentionally use non-zero exits for
//! domain-negative answers. This module centralizes that knowledge so
//! executors and harnesses stop treating every non-zero as an execution
//! failure.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitSemantics {
    Success,
    /// Command completed normally but answered "no" in its domain
    /// (e.g. `grep` no matches).
    InformationalFailure,
    /// Command completed normally and found a negative/different
    /// domain state (e.g. `diff` found differences, `test` false).
    DomainNegative,
    /// Command failed to execute, crashed, was denied, or returned a
    /// tool-specific real error.
    ExecutionError,
}

impl ExitSemantics {
    #[must_use]
    pub fn is_tool_error(self) -> bool {
        matches!(self, Self::ExecutionError)
    }
}

#[must_use]
pub fn classify_exit(command: &str, exit_code: i32) -> ExitSemantics {
    if exit_code == 0 {
        return ExitSemantics::Success;
    }
    if matches!(exit_code, 126 | 127) || !(0..128).contains(&exit_code) {
        return ExitSemantics::ExecutionError;
    }

    let family = command_family(command);
    match (family.as_deref(), exit_code) {
        (Some("grep" | "rg" | "ripgrep" | "ag"), 1) => ExitSemantics::InformationalFailure,
        (Some("diff" | "cmp"), 1) => ExitSemantics::DomainNegative,
        (Some("test" | "["), 1) => ExitSemantics::DomainNegative,
        // `git diff --quiet` intentionally returns 1 when changes exist.
        (Some("git"), 1) if command_contains_word(command, "diff") => ExitSemantics::DomainNegative,
        _ => ExitSemantics::ExecutionError,
    }
}

fn command_family(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace().peekable();
    while let Some(token) = tokens.peek() {
        if is_env_assignment(token) {
            tokens.next();
        } else {
            break;
        }
    }
    let family = tokens
        .next()
        .map(|s| s.trim_matches('"').to_ascii_lowercase())?;
    if is_test_runner_family(&family) && command_contains_word(command, "test") {
        return Some("test".to_string());
    }
    Some(family)
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !name.as_bytes()[0].is_ascii_digit()
}

fn command_contains_word(command: &str, needle: &str) -> bool {
    command
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .any(|word| word == needle)
}

fn is_test_runner_family(family: &str) -> bool {
    matches!(
        family,
        "cargo" | "go" | "npm" | "pnpm" | "yarn" | "bun" | "pytest" | "uv" | "poetry"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_no_match_is_informational_not_execution_error() {
        let semantics = classify_exit("grep missing src/main.rs", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn diff_and_test_false_are_domain_negative() {
        for command in [
            "diff a b",
            "git diff --quiet",
            "test -f missing",
            "[ -f missing ]",
            "cargo test",
            "go test ./...",
            "npm test",
            "pnpm test",
        ] {
            let semantics = classify_exit(command, 1);
            assert_eq!(semantics, ExitSemantics::DomainNegative, "{command}");
            assert!(!semantics.is_tool_error(), "{command}");
        }
    }

    #[test]
    fn command_not_found_and_signal_are_execution_errors() {
        for code in [2, 126, 127, 130, -1] {
            let semantics = classify_exit("grep bad[", code);
            assert_eq!(semantics, ExitSemantics::ExecutionError, "{code}");
            assert!(semantics.is_tool_error(), "{code}");
        }
    }

    #[test]
    fn leading_env_assignment_does_not_hide_command_family() {
        assert_eq!(
            classify_exit("LC_ALL=C grep needle file", 1),
            ExitSemantics::InformationalFailure
        );
    }
}
