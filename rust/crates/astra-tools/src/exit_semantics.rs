//! Semantic classification for process exit codes.
//!
//! POSIX exit codes are not a boolean success/failure API. Tools like
//! `grep`, `diff`, and `test` intentionally use non-zero exits for semantic
//! non-success answers. This module centralizes that knowledge so executors
//! and harnesses stop treating every non-zero as an execution failure.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitSemantics {
    Success,
    /// Command completed normally but did not find a requested informational
    /// target (e.g. `grep`/`rg` no matches, `pgrep` no processes).
    InformationalFailure,
    /// Command completed normally and found a negative/different domain state
    /// (e.g. `diff`/`cmp` found differences, `test` false).
    DomainNegative,
    /// Command was terminated because the tool timeout elapsed.
    TimedOut,
    /// Command was terminated because the user/run cancellation token fired.
    Cancelled,
    /// Command was terminated by a signal. POSIX shells conventionally surface
    /// this as exit status `128 + signal`.
    Signaled,
    /// Command failed to execute, crashed, was denied, or returned a
    /// tool-specific real error.
    ExecutionError,
}

impl ExitSemantics {
    #[must_use]
    pub fn is_tool_error(self) -> bool {
        matches!(
            self,
            Self::TimedOut | Self::Cancelled | Self::Signaled | Self::ExecutionError
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResultClass {
    Success,
    DomainNegative,
    TestFailure,
    EnvFailure,
    ExecutionError,
    Inconclusive,
}

impl CommandResultClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::DomainNegative => "domain_negative",
            Self::TestFailure => "test_failure",
            Self::EnvFailure => "env_failure",
            Self::ExecutionError => "execution_error",
            Self::Inconclusive => "inconclusive",
        }
    }

    #[must_use]
    pub fn is_tool_error(self) -> bool {
        matches!(self, Self::EnvFailure | Self::ExecutionError)
    }
}

#[must_use]
pub fn classify_exit(command: &str, exit_code: i32) -> ExitSemantics {
    if exit_code == 0 {
        return ExitSemantics::Success;
    }
    if (128..256).contains(&exit_code) {
        return ExitSemantics::Signaled;
    }
    if matches!(exit_code, 126 | 127) || !(0..128).contains(&exit_code) {
        return ExitSemantics::ExecutionError;
    }
    if exit_code == 1
        && let Some(semantics) = pipeline_domain_negative_semantics(command)
    {
        return semantics;
    }

    let family = command_family(command);
    match (family.as_deref(), exit_code) {
        (Some("grep" | "rg" | "ripgrep" | "ag"), 1) => ExitSemantics::InformationalFailure,
        (Some("git"), 1) if command_contains_word(command, "grep") => {
            ExitSemantics::InformationalFailure
        }
        (Some("pgrep" | "pkill" | "killall"), 1) => ExitSemantics::InformationalFailure,
        (Some("diff" | "cmp"), 1) => ExitSemantics::DomainNegative,
        (Some("false"), 1) => ExitSemantics::DomainNegative,
        (Some("test" | "["), 1) => ExitSemantics::DomainNegative,
        (Some("pytest" | "nose2" | "tox" | "unittest" | "jest" | "vitest" | "mocha"), 1) => {
            ExitSemantics::DomainNegative
        }
        // `git diff --quiet` intentionally returns 1 when changes exist.
        (Some("git"), 1) if command_contains_word(command, "diff") => ExitSemantics::DomainNegative,
        _ => ExitSemantics::ExecutionError,
    }
}

#[must_use]
pub fn classify_command_result(
    command: &str,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
) -> CommandResultClass {
    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let lower = combined.to_ascii_lowercase();

    if looks_like_env_failure(&lower) {
        return CommandResultClass::EnvFailure;
    }

    if is_build_test_or_lint_command(command) && looks_like_build_or_test_failure(&lower) {
        return CommandResultClass::TestFailure;
    }

    match exit_code {
        Some(code) => match classify_exit(command, code) {
            ExitSemantics::Success => CommandResultClass::Success,
            ExitSemantics::InformationalFailure | ExitSemantics::DomainNegative => {
                if is_build_test_or_lint_command(command) {
                    CommandResultClass::TestFailure
                } else {
                    CommandResultClass::DomainNegative
                }
            }
            ExitSemantics::TimedOut | ExitSemantics::Cancelled | ExitSemantics::Signaled => {
                CommandResultClass::ExecutionError
            }
            ExitSemantics::ExecutionError => {
                if is_build_test_or_lint_command(command)
                    && looks_like_build_or_test_failure(&lower)
                {
                    CommandResultClass::TestFailure
                } else {
                    CommandResultClass::ExecutionError
                }
            }
        },
        None => {
            if looks_like_build_or_test_failure(&lower) {
                CommandResultClass::TestFailure
            } else {
                CommandResultClass::Inconclusive
            }
        }
    }
}

/// Return the last segment of a shell pipeline.
///
/// This is a small shell lexer, not a full parser: it tracks quotes and
/// backslash escapes so only an unquoted `|` separates pipeline commands.
/// That covers the cases exit semantics care about (`grep -E 'a|b'`,
/// `python -c "print('a|b')" | head`) without treating regex alternation as a
/// pipeline.
pub fn last_pipeline_segment(command: &str) -> &str {
    split_pipeline_segments(command)
        .last()
        .copied()
        .unwrap_or(command)
}

fn split_pipeline_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !in_single && i + 1 < bytes.len() => {
                i += 2;
                continue;
            }
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'|' if !in_single && !in_double => {
                let prev_pipe = i > 0 && bytes[i - 1] == b'|';
                let next_pipe = i + 1 < bytes.len() && bytes[i + 1] == b'|';
                if !prev_pipe && !next_pipe {
                    segments.push(command[start..i].trim());
                    start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    segments.push(command[start..].trim());
    segments
}

#[must_use]
pub fn command_family(command: &str) -> Option<String> {
    // Extract the *last* command in a pipeline — the last segment determines
    // the exit code, not the first (e.g. `ls | grep foo` → `grep`).
    // Skip escaped `\|` used in regex patterns like `grep 'foo\|bar'`.
    segment_family(last_pipeline_segment(command))
}

fn segment_family(segment: &str) -> Option<String> {
    let mut tokens = last_shell_list_segment(segment)
        .split_whitespace()
        .skip_while(|t| is_env_assignment(t));
    let family = tokens
        .next()
        .map(|s| s.trim_matches('"').to_ascii_lowercase())?;
    if matches!(family.as_str(), "python" | "python3" | "uv" | "poetry") {
        let mut peek = tokens.clone();
        if peek.next() == Some("-m")
            && let Some(module) = peek.next()
        {
            let module = module.trim_matches('"').to_ascii_lowercase();
            if matches!(module.as_str(), "pytest" | "unittest") {
                return Some(module);
            }
        }
    }
    if is_test_runner_family(&family) && command_contains_word(segment, "test") {
        return Some("test".to_string());
    }
    Some(family)
}

fn pipeline_domain_negative_semantics(command: &str) -> Option<ExitSemantics> {
    let segments = split_pipeline_segments(command);
    if segments.len() < 2 {
        return None;
    }
    // With pipefail, the exit code can come from any segment. Classify based
    // on the last segment first (it always determines the exit code without
    // pipefail). For earlier segments, only attribute the exit code to them
    // if the last segment is a passive sink (head, tail, tee, sort, wc, etc.)
    // that doesn't independently produce exit 1.
    let last = segments.last().unwrap();
    match segment_family(last).as_deref() {
        Some("grep" | "rg" | "ripgrep" | "ag") => {
            return Some(ExitSemantics::InformationalFailure);
        }
        Some("git") if command_contains_word(last, "grep") => {
            return Some(ExitSemantics::InformationalFailure);
        }
        Some("diff" | "cmp" | "false" | "test" | "[") => {
            return Some(ExitSemantics::DomainNegative);
        }
        Some("pytest" | "nose2" | "tox" | "unittest" | "jest" | "vitest" | "mocha") => {
            return Some(ExitSemantics::DomainNegative);
        }
        Some("git") if command_contains_word(last, "diff") => {
            return Some(ExitSemantics::DomainNegative);
        }
        _ => {}
    }

    // Last segment is not a known domain-negative tool. With pipefail, exit 1
    // could come from an earlier search/test segment IF the last segment is a
    // passive data sink that doesn't independently fail with exit 1.
    if is_passive_pipe_sink(last) {
        for segment in &segments[..segments.len() - 1] {
            match segment_family(segment).as_deref() {
                Some("grep" | "rg" | "ripgrep" | "ag") => {
                    return Some(ExitSemantics::InformationalFailure);
                }
                Some("git") if command_contains_word(segment, "grep") => {
                    return Some(ExitSemantics::InformationalFailure);
                }
                Some("diff" | "cmp" | "false" | "test" | "[") => {
                    return Some(ExitSemantics::DomainNegative);
                }
                Some("git") if command_contains_word(segment, "diff") => {
                    return Some(ExitSemantics::DomainNegative);
                }
                _ => {}
            }
        }
    }
    None
}

fn is_passive_pipe_sink(segment: &str) -> bool {
    matches!(
        segment_family(segment).as_deref(),
        Some(
            "head"
                | "tail"
                | "tee"
                | "sort"
                | "uniq"
                | "wc"
                | "cat"
                | "less"
                | "more"
                | "cut"
                | "tr"
                | "sed"
                | "awk"
                | "column"
                | "fmt"
                | "fold"
                | "nl"
                | "paste"
                | "rev"
                | "expand"
                | "unexpand"
        )
    )
}

fn last_shell_list_segment(command: &str) -> &str {
    let bytes = command.as_bytes();
    let mut last_start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !in_single && i + 1 < bytes.len() => {
                i += 2;
                continue;
            }
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b';' if !in_single && !in_double => {
                last_start = i + 1;
            }
            b'&' if !in_single && !in_double && i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                last_start = i + 2;
                i += 1;
            }
            b'|' if !in_single && !in_double && i + 1 < bytes.len() && bytes[i + 1] == b'|' => {
                last_start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    command[last_start..].trim()
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

fn is_build_test_or_lint_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo build",
        "pytest",
        "python -m pytest",
        "python3 -m pytest",
        "python -m unittest",
        "python3 -m unittest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "jest",
        "vitest",
        "go test",
        "go build",
        "make test",
        "make check",
        "mypy",
        "pyright",
        "eslint",
        "ruff check",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_env_failure(lower_output: &str) -> bool {
    [
        "command not found",
        "no such file or directory",
        "modulenotfounderror: no module named",
        "importerror: no module named",
        "could not find a version that satisfies the requirement",
        "failed to create virtual environment",
        "error: externally-managed-environment",
        "no python at",
    ]
    .iter()
    .any(|needle| lower_output.contains(needle))
}

fn looks_like_build_or_test_failure(lower_output: &str) -> bool {
    [
        "test result: failed",
        "test failed",
        "tests failed",
        "test suite failed",
        " failed,",
        " failures:",
        "error[",
        "traceback (most recent call last)",
        "assertionerror",
        "failed tests",
        "test suites:",
        "--- fail:",
        "build failed",
        "could not compile",
    ]
    .iter()
    .any(|needle| lower_output.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{CommandResultClass, ExitSemantics, classify_command_result, classify_exit};

    #[test]
    fn grep_no_match_is_informational_failure() {
        for cmd in &[
            "grep needle missing",
            "grep missing src/main.rs",
            "rg needle src",
            "ripgrep needle src",
            "ag needle src",
        ] {
            let sem = classify_exit(cmd, 1);
            assert_eq!(sem, ExitSemantics::InformationalFailure, "cmd={cmd}");
            assert!(!sem.is_tool_error(), "cmd={cmd}");
        }
    }

    #[test]
    fn cd_wrapped_grep_no_match_is_informational() {
        let semantics = classify_exit("cd /work/repo && grep -n missing src/main.rs", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn grep_pipeline_no_match_remains_informational_under_pipefail() {
        let semantics = classify_exit("grep needle haystack.txt | head -20", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn non_domain_pipeline_failure_is_execution_error() {
        let semantics = classify_exit("sh -c 'exit 7' | head -20", 7);
        assert_eq!(semantics, ExitSemantics::ExecutionError);
        assert!(semantics.is_tool_error());
    }

    #[test]
    fn git_grep_no_match_is_informational() {
        let semantics = classify_exit("git grep missing -- src", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn process_match_commands_no_match_are_informational() {
        for command in [
            "pgrep missing-process-name",
            "cd /work/repo && pgrep missing-process-name",
            "pkill -0 missing-process-name",
            "killall -0 missing-process-name",
        ] {
            let semantics = classify_exit(command, 1);
            assert_eq!(semantics, ExitSemantics::InformationalFailure, "{command}");
            assert!(!semantics.is_tool_error(), "{command}");
        }
    }

    #[test]
    fn grep_escaped_pipe_no_match_is_informational() {
        // Regression: `grep 'version\|Version' file` has an escaped `|` in the
        // regex, not a pipeline separator. It must still be classified as grep.
        let semantics = classify_exit("grep -n 'version\\|Version' crates/foo/src/file.rs", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn grep_quoted_regex_pipe_no_match_is_informational() {
        for command in [
            "grep -E 'foo|bar' crates/foo/src/file.rs",
            "grep -E \"foo|bar\" crates/foo/src/file.rs",
        ] {
            let semantics = classify_exit(command, 1);
            assert_eq!(semantics, ExitSemantics::InformationalFailure, "{command}");
            assert!(!semantics.is_tool_error(), "{command}");
        }
    }

    #[test]
    fn pipeline_last_command_determines_exit_semantics() {
        // `ls | grep foo`: exit code comes from grep, not ls.
        let semantics = classify_exit("ls -la | grep missing", 1);
        assert_eq!(semantics, ExitSemantics::InformationalFailure);
        assert!(!semantics.is_tool_error());
    }

    #[test]
    fn diff_test_false_and_false_are_domain_negative() {
        for command in [
            "diff a b",
            "cmp a b",
            "git diff --quiet",
            "git diff --exit-code",
            "test -f missing",
            "[ -f missing ]",
            "false",
            "cargo test",
            "go test ./...",
            "npm test",
            "pnpm test",
            "pytest tests/",
            "python -m pytest tests/",
        ] {
            let semantics = classify_exit(command, 1);
            assert_eq!(semantics, ExitSemantics::DomainNegative, "{command}");
            assert!(!semantics.is_tool_error(), "{command}");
        }
    }

    #[test]
    fn command_not_found_is_execution_error() {
        for code in [2, 126, 127, -1] {
            let semantics = classify_exit("grep bad[", code);
            assert_eq!(semantics, ExitSemantics::ExecutionError, "{code}");
            assert!(semantics.is_tool_error(), "{code}");
        }
    }

    #[test]
    fn signal_encoded_exit_is_signaled() {
        for code in [129, 130, 137, 143] {
            let semantics = classify_exit("sleep 999", code);
            assert_eq!(semantics, ExitSemantics::Signaled, "{code}");
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

    #[test]
    fn command_result_classification_cases() {
        // env failure even when exit zero
        let class = classify_command_result(
            "python -m pytest tests 2>&1 | tail -20",
            "bash: python: command not found\n",
            "",
            Some(0),
        );
        assert_eq!(class, CommandResultClass::EnvFailure);
        assert!(class.is_tool_error());

        // masked test failure in output
        let class = classify_command_result(
            "cargo test --lib",
            "test result: FAILED. 0 passed; 1 failed",
            "",
            Some(0),
        );
        assert_eq!(class, CommandResultClass::TestFailure);
        assert!(!class.is_tool_error());

        // grep no-match is a semantic non-error outcome. The coarse result
        // class stays domain_negative; exit_semantics keeps the finer
        // informational_failure distinction.
        let class = classify_command_result("grep needle missing", "", "", Some(1));
        assert_eq!(class, CommandResultClass::DomainNegative);
        assert!(!class.is_tool_error());
    }

    #[test]
    fn command_result_detects_masked_test_failure() {
        let class = classify_command_result(
            "cargo test 2>&1 | tail -20",
            "test result: FAILED. 1 passed; 1 failed\n",
            "",
            Some(0),
        );
        assert_eq!(class, CommandResultClass::TestFailure);
        assert!(!class.is_tool_error());
    }

    #[test]
    fn command_result_keeps_grep_no_match_non_error() {
        let class = classify_command_result("grep needle missing", "", "", Some(1));
        assert_eq!(class, CommandResultClass::DomainNegative);
        assert!(!class.is_tool_error());
    }
}
