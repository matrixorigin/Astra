//! Git safety validation for shell commands.
//!
//! Provides defense-in-depth checks that run *before* a git command is executed.

use std::path::Path;

/// Result of a git safety check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSafetyViolation {
    /// Commit message contains command substitution (`$()`, backticks, `${}`).
    CommitMessageInjection { pattern: &'static str },
    /// Commit message starts with `-` (argument injection).
    CommitMessageDash,
    /// Command uses `--no-verify` or similar hook-skip flags.
    HookSkipFlag { flag: &'static str },
    /// Force push detected (`--force`, `-f`, `--force-with-lease`).
    ForcePush,
    /// Force push to a protected branch (main, master, develop).
    ForcePushProtectedBranch { branch: String },
    /// Compound command chains `cd` with `git` (bare repo attack vector).
    CdGitCompound,
    /// `git -c` used (arbitrary config = code execution via core.fsmonitor, diff.external, etc.).
    GitConfigFlag,
    /// `git --exec-path` or `--config-env` used (execution path manipulation).
    GitExecPathFlag,
    /// `git commit --amend` without explicit user request.
    CommitAmend,
    /// Current directory looks like a bare git repo (potential hook execution trap).
    BareRepoDetected,
}

impl std::fmt::Display for GitSafetyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommitMessageInjection { pattern } => {
                write!(
                    f,
                    "commit message contains shell injection pattern: {pattern}"
                )
            }
            Self::CommitMessageDash => {
                write!(f, "commit message starts with '-' (argument injection)")
            }
            Self::HookSkipFlag { flag } => {
                write!(f, "hook-skip flag '{flag}' requires explicit user approval")
            }
            Self::ForcePush => write!(f, "force push requires explicit user approval"),
            Self::ForcePushProtectedBranch { branch } => {
                write!(f, "force push to protected branch '{branch}' blocked")
            }
            Self::CdGitCompound => {
                write!(
                    f,
                    "cd + git compound command blocked (bare repo attack vector)"
                )
            }
            Self::GitConfigFlag => {
                write!(f, "git -c flag blocked (arbitrary config = code execution)")
            }
            Self::GitExecPathFlag => {
                write!(
                    f,
                    "git --exec-path/--config-env blocked (path manipulation)"
                )
            }
            Self::CommitAmend => {
                write!(f, "git commit --amend requires explicit user approval")
            }
            Self::BareRepoDetected => {
                write!(f, "current directory appears to be a bare git repo")
            }
        }
    }
}

/// Whether a violation is "soft" — respects auto-run mode and session overrides.
///
/// Soft violations are common legitimate operations (e.g. `cd repo && git status`)
/// that should not repeatedly prompt the user after they chose auto-run.
/// Hard violations (injection, config manipulation) always require explicit approval.
pub fn is_soft_violation(v: &GitSafetyViolation) -> bool {
    matches!(
        v,
        GitSafetyViolation::ForcePush
            | GitSafetyViolation::CdGitCompound
            | GitSafetyViolation::CommitAmend
    )
}

/// Validate a shell command for git safety violations.
///
/// Returns all detected violations (may be multiple per command).
pub fn validate_git_command(command: &str) -> Vec<GitSafetyViolation> {
    let mut violations = Vec::new();
    let lower = command.to_lowercase();

    // Only check commands that involve git (as a standalone word).
    let is_git_command = lower.split_whitespace().any(|w| w == "git")
        || lower.split_whitespace().any(|w| w.ends_with("/git"));
    if !is_git_command {
        return violations;
    }

    check_commit_message(command, &mut violations);
    check_hook_skip_flags(&lower, &mut violations);
    check_force_push(&lower, &mut violations);
    check_cd_git_compound(&lower, &mut violations);
    check_git_config_flags(command, &mut violations);
    check_commit_amend(&lower, &mut violations);

    violations
}

/// Check if a directory looks like a bare git repo (attack vector for hook execution).
///
/// A bare repo has HEAD, objects/, and refs/ at the top level without a `.git/HEAD`.
pub fn is_bare_git_repo(dir: &Path) -> bool {
    let head = dir.join("HEAD");
    let objects = dir.join("objects");
    let refs = dir.join("refs");
    let dot_git_head = dir.join(".git").join("HEAD");

    head.is_file() && objects.is_dir() && refs.is_dir() && !dot_git_head.is_file()
}

// --- Internal helpers ---

fn check_commit_message(command: &str, violations: &mut Vec<GitSafetyViolation>) {
    let Some(m_pos) = command.find("-m ") else {
        return;
    };
    if !command[..m_pos].contains("commit") {
        return;
    }

    let after_m = &command[m_pos + 3..];

    let (msg, is_double_quoted) = if after_m.starts_with('"') {
        (extract_quoted(after_m, '"'), true)
    } else if after_m.starts_with('\'') {
        (extract_quoted(after_m, '\''), false)
    } else {
        (after_m.split_whitespace().next().unwrap_or(""), false)
    };

    // Block command substitution patterns inside double-quoted messages only.
    // Single-quoted messages are safe (shell doesn't expand inside single quotes).
    if is_double_quoted {
        for (pattern, label) in [("$(", "$(...)"), ("`", "backtick"), ("${", "${...}")] {
            if msg.contains(pattern) {
                // Allow $(cat << ...) — common heredoc-based multi-line
                // commit message pattern that reads from a literal block.
                if pattern == "$(" && (msg.starts_with("$(cat <<") || msg.starts_with("$(< ")) {
                    break;
                }
                violations.push(GitSafetyViolation::CommitMessageInjection { pattern: label });
                return;
            }
        }
    }

    // Block messages starting with `-` (argument injection).
    if msg.starts_with('-') {
        violations.push(GitSafetyViolation::CommitMessageDash);
    }
}

fn check_hook_skip_flags(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    // Only --no-verify is a safety concern (skips pre-commit/pre-push hooks).
    // --no-gpg-sign and --no-signoff are not safety hooks.
    if lower.contains("--no-verify") {
        violations.push(GitSafetyViolation::HookSkipFlag {
            flag: "--no-verify",
        });
    }
}

fn check_force_push(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("push") {
        return;
    }

    let words: Vec<&str> = lower.split_whitespace().collect();

    // Detect force push flags precisely: --force, --force-with-lease, or bare -f
    let is_force = words
        .iter()
        .any(|&w| w == "--force" || w == "--force-with-lease" || w == "-f");
    if !is_force {
        return;
    }
    violations.push(GitSafetyViolation::ForcePush);

    // Check for protected branches (main, master, develop, production, staging).
    // Match whole branch name or the final component after '/' to avoid
    // false positives on branches like "feature/main-refactor".
    let protected_branches = ["main", "master", "develop", "production", "staging"];

    for (i, word) in words.iter().enumerate() {
        if (*word == "origin" || *word == "push")
            && let Some(next) = words.get(i + 1)
        {
            if next.starts_with('-') {
                continue;
            }
            // Extract the final path component (e.g. "main" from "origin/main")
            let branch_leaf = next.rsplit('/').next().unwrap_or(next);
            for protected in &protected_branches {
                if branch_leaf == *protected {
                    violations.push(GitSafetyViolation::ForcePushProtectedBranch {
                        branch: next.to_string(),
                    });
                    return;
                }
            }
        }
    }
}

fn check_cd_git_compound(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    let has_cd = lower.contains("cd ");
    let has_git = lower.contains("git ");
    let is_compound = lower.contains("&&") || lower.contains("||") || lower.contains(';');

    if has_cd && has_git && is_compound {
        violations.push(GitSafetyViolation::CdGitCompound);
    }
}

fn check_git_config_flags(command: &str, violations: &mut Vec<GitSafetyViolation>) {
    // Scan all words for dangerous git global flags.
    // We don't try to parse git's complex flag grammar — just look for the dangerous
    // flags anywhere after "git" and before common subcommands.
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, &word) in words.iter().enumerate() {
        if word == "git" || word.ends_with("/git") {
            // Scan all subsequent words (don't stop at non-flag args, because
            // flags like -C take a path argument that doesn't start with -).
            for &next in words.iter().skip(i + 1) {
                if next == "-c" || next.starts_with("-c=") || next.starts_with("--config=") {
                    violations.push(GitSafetyViolation::GitConfigFlag);
                    return;
                }
                if next == "--exec-path" || next.starts_with("--exec-path=") {
                    violations.push(GitSafetyViolation::GitExecPathFlag);
                    return;
                }
                if next == "--config-env" || next.starts_with("--config-env=") {
                    violations.push(GitSafetyViolation::GitExecPathFlag);
                    return;
                }
            }
        }
    }
}

fn check_commit_amend(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if lower.contains("commit") && lower.contains("--amend") {
        violations.push(GitSafetyViolation::CommitAmend);
    }
}

/// Extract content between matching quote characters (simple, non-recursive).
fn extract_quoted(s: &str, quote: char) -> &str {
    if !s.starts_with(quote) {
        return "";
    }
    let inner = &s[1..];
    match inner.find(quote) {
        Some(end) => &inner[..end],
        None => inner, // unterminated quote — return rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Commit message injection ---

    #[test]
    fn commit_message_injection_patterns() {
        for cmd in [
            r#"git commit -m "$(whoami) was here""#,
            "git commit -m \"`id` commit\"",
            r#"git commit -m "${HOME} commit""#,
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|v| matches!(v, GitSafetyViolation::CommitMessageInjection { .. })),
                "should block: {cmd}"
            );
        }
    }

    #[test]
    fn safe_commit_messages_allowed() {
        // Single quotes prevent expansion; normal messages are safe
        for cmd in [
            "git commit -m '$(whoami) was here'",
            "git commit -m 'fix: resolve null pointer'",
        ] {
            let v = validate_git_command(cmd);
            assert!(v.is_empty(), "should allow: {cmd}");
        }
    }

    // --- Hook skip flags ---

    #[test]
    fn blocks_no_verify() {
        let v = validate_git_command("git commit --no-verify -m 'skip hooks'");
        assert!(v.iter().any(|v| matches!(
            v,
            GitSafetyViolation::HookSkipFlag {
                flag: "--no-verify"
            }
        )));
    }

    // --- Force push ---

    #[test]
    fn force_push_behavior() {
        // Detect force push
        for (cmd, is_protected) in [
            ("git push --force origin main", true),
            ("git push -f origin main", false),
            ("git push --force origin master", true),
        ] {
            let v = validate_git_command(cmd);
            assert!(v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePush)), "ForcePush for: {cmd}");
            if is_protected {
                assert!(v.iter().any(|violation| matches!(
                    violation, GitSafetyViolation::ForcePushProtectedBranch { .. }
                )), "ForcePushProtectedBranch for: {cmd}");
            }
        }
        // Feature branch NOT protected
        let v = validate_git_command("git push --force origin feature/my-feature");
        assert!(!v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePushProtectedBranch { .. })));
        // Feature branches containing "main"/"develop" are NOT protected (false positive regression)
        for cmd in ["git push --force origin feature/main-refactor", "git push -f origin feature/develop-ui"] {
            let v = validate_git_command(cmd);
            assert!(v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePush)));
            assert!(!v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePushProtectedBranch { .. })), "false positive for {cmd}");
        }
        // "origin/main" with remote prefix IS protected
        let v = validate_git_command("git push --force origin origin/main");
        assert!(v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePushProtectedBranch { .. })));
        // Non-force flags are NOT force push
        for cmd in ["git push --follow-tags origin my-branch", "git push -ff origin my-branch"] {
            let v = validate_git_command(cmd);
            assert!(!v.iter().any(|violation| matches!(violation, GitSafetyViolation::ForcePush)), "false positive: {cmd}");
        }
        // --force-with-lease IS force push
        let v = validate_git_command("git push --force-with-lease");
        assert!(v.iter().any(|v| matches!(v, GitSafetyViolation::ForcePush)));
        // Path-prefixed git
        let v = validate_git_command("/usr/bin/git push --force");
        assert!(v.iter().any(|v| matches!(v, GitSafetyViolation::ForcePush)));
    }

    #[test]
    fn cd_git_compound_behavior() {
        let v = validate_git_command("cd /tmp/evil && git status");
        assert!(v.iter().any(|v| matches!(v, GitSafetyViolation::CdGitCompound)));
        let v = validate_git_command("git status");
        assert!(v.is_empty());
    }

    // --- git -c config injection ---

    #[test]
    fn git_config_flags_blocked() {
        for cmd in [
            "git -c core.fsmonitor=evil status",
            "git --exec-path=/tmp/evil status",
            "git --config-env=core.editor=EDITOR commit",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter().any(|violation| matches!(
                    violation,
                    GitSafetyViolation::GitConfigFlag | GitSafetyViolation::GitExecPathFlag
                )),
                "should block: {cmd}"
            );
        }
    }

    // --- commit --amend ---

    #[test]
    fn blocks_commit_amend() {
        let v = validate_git_command("git commit --amend -m 'rewrite'");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitAmend))
        );
    }

    // --- bare repo detection ---

    #[test]
    fn bare_repo_detection() {
        // Bare: HEAD + objects + refs at top level, no .git
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(dir.path().join("objects")).unwrap();
        std::fs::create_dir(dir.path().join("refs")).unwrap();
        assert!(is_bare_git_repo(dir.path()));

        // Normal: .git/HEAD + objects + refs
        let dir2 = tempfile::tempdir().unwrap();
        let git_dir = dir2.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(dir2.path().join("objects")).unwrap();
        std::fs::create_dir(dir2.path().join("refs")).unwrap();
        assert!(!is_bare_git_repo(dir2.path()));

        // Empty dir
        let dir3 = tempfile::tempdir().unwrap();
        assert!(!is_bare_git_repo(dir3.path()));
    }

    // --- validate_git_command edge cases ---

    #[test]
    fn non_git_commands_return_empty() {
        for cmd in ["echo hello", ""] {
            let violations = validate_git_command(cmd);
            assert!(violations.is_empty(), "should be empty for: {cmd}");
        }
    }

    #[test]
    fn multiple_violations_in_one_command() {
        let violations = validate_git_command("git commit --amend --no-verify");
        assert!(
            violations.len() >= 2,
            "should detect both amend and no-verify"
        );
    }

    #[test]
    fn violation_display_all_variants() {
        // Ensure Display impl doesn't panic for any variant
        let violations = vec![
            GitSafetyViolation::CommitMessageInjection { pattern: "$()" },
            GitSafetyViolation::CommitMessageDash,
            GitSafetyViolation::HookSkipFlag {
                flag: "--no-verify",
            },
            GitSafetyViolation::ForcePush,
            GitSafetyViolation::ForcePushProtectedBranch {
                branch: "main".into(),
            },
            GitSafetyViolation::CdGitCompound,
            GitSafetyViolation::GitConfigFlag,
            GitSafetyViolation::GitExecPathFlag,
            GitSafetyViolation::CommitAmend,
            GitSafetyViolation::BareRepoDetected,
        ];
        for v in &violations {
            let msg = format!("{v}");
            assert!(!msg.is_empty());
        }
    }
}
