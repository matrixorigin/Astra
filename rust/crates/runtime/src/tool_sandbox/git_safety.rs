//! Git safety validation for shell commands.
//!
//! Inspired by Claude Code's `bashSecurity.ts`, `readOnlyValidation.ts`, and `gitSafety.ts`.
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
    let is_force =
        lower.contains("--force") || lower.contains(" -f") || lower.contains("--force-with-lease");
    if !is_force {
        return;
    }
    violations.push(GitSafetyViolation::ForcePush);

    // Check for protected branches (main, master, develop, release/*)
    let protected_branches = ["main", "master", "develop", "production", "staging"];
    let words: Vec<&str> = lower.split_whitespace().collect();

    // Look for branch name after "origin" or after "push"
    for (i, word) in words.iter().enumerate() {
        if *word == "origin" || *word == "push" {
            if let Some(next) = words.get(i + 1) {
                // Skip flags
                if next.starts_with('-') {
                    continue;
                }
                // Check against protected branches
                for protected in &protected_branches {
                    if next.contains(protected) {
                        violations.push(GitSafetyViolation::ForcePushProtectedBranch {
                            branch: next.to_string(),
                        });
                        return;
                    }
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
    fn blocks_command_substitution_in_commit() {
        let v = validate_git_command(r#"git commit -m "$(whoami) was here""#);
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitMessageInjection { .. }))
        );
    }

    #[test]
    fn blocks_backtick_in_commit() {
        let v = validate_git_command("git commit -m \"`id` commit\"");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitMessageInjection { .. }))
        );
    }

    #[test]
    fn blocks_brace_expansion_in_commit() {
        let v = validate_git_command(r#"git commit -m "${HOME} commit""#);
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitMessageInjection { .. }))
        );
    }

    #[test]
    fn allows_single_quoted_commit() {
        // Single quotes prevent expansion — safe.
        let v = validate_git_command("git commit -m '$(whoami) was here'");
        assert!(v.is_empty());
    }

    #[test]
    fn blocks_dash_prefix_commit() {
        let v = validate_git_command("git commit -m \"-evil\"");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitMessageDash))
        );
    }

    #[test]
    fn allows_normal_commit() {
        let v = validate_git_command("git commit -m 'fix: resolve null pointer'");
        assert!(v.is_empty());
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
    fn blocks_force_push() {
        let v = validate_git_command("git push --force origin main");
        assert!(v.iter().any(|v| matches!(v, GitSafetyViolation::ForcePush)));
    }

    #[test]
    fn blocks_force_push_short() {
        let v = validate_git_command("git push -f origin main");
        assert!(v.iter().any(|v| matches!(v, GitSafetyViolation::ForcePush)));
    }

    #[test]
    fn blocks_force_push_protected_branch() {
        let v = validate_git_command("git push --force origin main");
        assert!(v
            .iter()
            .any(|v| matches!(v, GitSafetyViolation::ForcePushProtectedBranch { branch } if branch == "main")));

        let v = validate_git_command("git push --force origin master");
        assert!(v
            .iter()
            .any(|v| matches!(v, GitSafetyViolation::ForcePushProtectedBranch { branch } if branch == "master")));

        // Feature branch should not trigger protected branch violation
        let v = validate_git_command("git push --force origin feature/my-feature");
        assert!(!v
            .iter()
            .any(|v| matches!(v, GitSafetyViolation::ForcePushProtectedBranch { .. })));
    }

    // --- cd + git compound ---

    #[test]
    fn blocks_cd_git_compound() {
        let v = validate_git_command("cd /tmp/evil && git status");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CdGitCompound))
        );
    }

    #[test]
    fn allows_git_without_cd() {
        let v = validate_git_command("git status");
        assert!(v.is_empty());
    }

    // --- git -c config injection ---

    #[test]
    fn blocks_git_c_flag() {
        let v = validate_git_command("git -c core.fsmonitor=evil status");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::GitConfigFlag))
        );
    }

    #[test]
    fn blocks_git_exec_path() {
        let v = validate_git_command("git --exec-path=/tmp/evil status");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::GitExecPathFlag))
        );
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
    fn detects_bare_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(dir.path().join("objects")).unwrap();
        std::fs::create_dir(dir.path().join("refs")).unwrap();
        assert!(is_bare_git_repo(dir.path()));
    }

    #[test]
    fn normal_repo_not_bare() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(dir.path().join("objects")).unwrap();
        std::fs::create_dir(dir.path().join("refs")).unwrap();
        // Has .git/HEAD → not bare
        assert!(!is_bare_git_repo(dir.path()));
    }

    #[test]
    fn empty_dir_not_bare() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_bare_git_repo(dir.path()));
    }
}
