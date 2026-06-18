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
    /// Command can discard tracked or untracked worktree changes.
    WorktreeDestructive { operation: &'static str },
    /// Current directory looks like a bare git repo (potential hook execution trap).
    BareRepoDetected,
    /// git rebase with --exec (arbitrary command execution).
    RebaseExec,
    /// git clone with --recurse-submodules (submodule hooks execute arbitrary code).
    CloneRecurseSubmodules,
    /// git submodule update --init (submodule hooks execute arbitrary code).
    SubmoduleUpdateInit,
    /// git -C or --git-dir used (boundary escape — operates on a different repo).
    GitBoundaryEscape { flag: &'static str },
    /// git branch -D (force delete — loses unreachable commits).
    BranchForceDelete,
    /// git stash drop or git stash clear (permanently deletes stashed work).
    StashDestructive { operation: &'static str },
    /// git tag -d (deletes a tag — can lose versioning anchor).
    TagDelete,
    /// git bisect run (executes arbitrary command at each step).
    BisectRun,
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
            Self::WorktreeDestructive { operation } => {
                write!(
                    f,
                    "{operation} can discard worktree changes and requires explicit user approval"
                )
            }
            Self::BareRepoDetected => {
                write!(f, "current directory appears to be a bare git repo")
            }
            Self::RebaseExec => {
                write!(f, "git rebase --exec executes arbitrary commands")
            }
            Self::CloneRecurseSubmodules => {
                write!(
                    f,
                    "git clone --recurse-submodules may execute untrusted hooks"
                )
            }
            Self::SubmoduleUpdateInit => {
                write!(f, "git submodule update --init may execute untrusted hooks")
            }
            Self::GitBoundaryEscape { flag } => {
                write!(f, "git {flag} bypasses working directory (boundary escape)")
            }
            Self::BranchForceDelete => {
                write!(f, "git branch -D permanently deletes a branch")
            }
            Self::StashDestructive { operation } => {
                write!(f, "git stash {operation} permanently deletes stashed work")
            }
            Self::TagDelete => {
                write!(f, "git tag -d deletes a tag and may lose versioning anchor")
            }
            Self::BisectRun => {
                write!(
                    f,
                    "git bisect run executes arbitrary commands at each bisect step"
                )
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
            | GitSafetyViolation::BranchForceDelete
            | GitSafetyViolation::StashDestructive { .. }
            | GitSafetyViolation::TagDelete
    )
}

/// Validate a shell command for git safety violations.
///
/// Returns all detected violations (may be multiple per command).
pub fn validate_git_command(command: &str) -> Vec<GitSafetyViolation> {
    let mut violations = Vec::new();
    let lower = command.to_lowercase();

    // Only check commands that involve git (as a standalone word).
    if !contains_git_invocation(&lower) {
        return violations;
    }

    check_commit_message(command, &mut violations);
    check_hook_skip_flags(&lower, &mut violations);
    check_force_push(&lower, &mut violations);
    check_cd_git_compound(&lower, &mut violations);
    check_git_config_flags(command, &mut violations);
    check_rebase_exec(command, &lower, &mut violations);
    check_clone_recurse_submodules(&lower, &mut violations);
    check_submodule_update_init(&lower, &mut violations);
    check_commit_amend(&lower, &mut violations);
    check_worktree_destructive(&lower, &mut violations);
    check_branch_force_delete(command, &mut violations);
    check_stash_destructive(&lower, &mut violations);
    check_tag_delete(&lower, &mut violations);
    check_bisect_run(&lower, &mut violations);

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
    let mut scan_from = 0;
    while let Some((flag_pos, value_pos)) = next_commit_message_flag(command, scan_from) {
        scan_from = value_pos.saturating_add(1);
        if !git_commit_appears_before(command, flag_pos) {
            continue;
        }

        let (msg, quote) = extract_message_value(command, value_pos);

        // Block shell expansion in double-quoted and unquoted messages.
        // Single-quoted messages are safe because the shell does not expand inside them.
        if quote != Some('\'') && commit_message_has_expansion(msg, violations) {
            return;
        }

        // Block messages starting with `-` (argument injection).
        if msg.starts_with('-') {
            violations.push(GitSafetyViolation::CommitMessageDash);
            return;
        }
    }
}

fn commit_message_has_expansion(msg: &str, violations: &mut Vec<GitSafetyViolation>) -> bool {
    for (pattern, label) in [("$(", "$(...)"), ("`", "backtick"), ("${", "${...}")] {
        if msg.contains(pattern) {
            // Allow $(cat << ...) — common heredoc-based multi-line
            // commit message pattern that reads from a literal block.
            if pattern == "$(" && (msg.starts_with("$(cat <<") || msg.starts_with("$(< ")) {
                return false;
            }
            violations.push(GitSafetyViolation::CommitMessageInjection { pattern: label });
            return true;
        }
    }
    false
}

fn next_commit_message_flag(command: &str, start: usize) -> Option<(usize, usize)> {
    let mut idx = start.min(command.len());
    while idx < command.len() {
        let rest = &command[idx..];
        if !is_shell_arg_start(command, idx) {
            idx += rest.chars().next().map(char::len_utf8).unwrap_or(1);
            continue;
        }

        if rest.starts_with("--message=") {
            return Some((idx, idx + "--message=".len()));
        }
        if let Some(after_flag) = rest.strip_prefix("--message")
            && after_flag
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_whitespace())
        {
            return Some((idx, skip_ascii_whitespace(command, idx + "--message".len())));
        }
        if rest.starts_with("-m") && !rest.starts_with("--") {
            let after_flag = idx + 2;
            let next = command[after_flag..].chars().next();
            if next.is_none() {
                return Some((idx, after_flag));
            }
            if next.is_some_and(|ch| ch.is_ascii_whitespace()) {
                return Some((idx, skip_ascii_whitespace(command, after_flag)));
            }
            return Some((idx, after_flag));
        }

        idx += rest.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    None
}

fn git_commit_appears_before(command: &str, end: usize) -> bool {
    let lower = command[..end].to_lowercase();
    shell_words(&lower).any(|word| word == "commit")
}

fn extract_message_value(command: &str, start: usize) -> (&str, Option<char>) {
    let start = start.min(command.len());
    let rest = &command[start..];
    if rest.starts_with('"') {
        return (extract_quoted(rest, '"'), Some('"'));
    }
    if rest.starts_with('\'') {
        return (extract_quoted(rest, '\''), Some('\''));
    }

    let end = rest
        .find(|ch: char| ch.is_ascii_whitespace() || matches!(ch, ';' | '&' | '|'))
        .unwrap_or(rest.len());
    (&rest[..end], None)
}

fn skip_ascii_whitespace(command: &str, mut idx: usize) -> usize {
    while idx < command.len()
        && command[idx..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace())
    {
        idx += command[idx..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    idx
}

fn is_shell_arg_start(command: &str, idx: usize) -> bool {
    idx == 0
        || command[..idx]
            .chars()
            .next_back()
            .is_none_or(|ch| ch.is_ascii_whitespace() || is_shell_operator_char(ch))
}

fn is_shell_operator_char(ch: char) -> bool {
    matches!(ch, ';' | '&' | '|' | '(' | ')' | '{' | '}')
}

fn shell_words(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|ch: char| ch.is_ascii_whitespace() || is_shell_operator_char(ch))
        .filter(|word| !word.is_empty())
}

fn contains_git_invocation(command: &str) -> bool {
    shell_words(command).any(word_is_git)
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
    let has_cd = shell_words(lower).any(|word| word == "cd");
    let has_git = contains_git_invocation(lower);
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
                if next == "-C" || next.starts_with("-C=") {
                    violations.push(GitSafetyViolation::GitBoundaryEscape { flag: "-C" });
                    return;
                }
                if next == "--git-dir" || next.starts_with("--git-dir=") {
                    violations.push(GitSafetyViolation::GitBoundaryEscape { flag: "--git-dir" });
                    return;
                }
                if next == "--work-tree" || next.starts_with("--work-tree=") {
                    violations.push(GitSafetyViolation::GitBoundaryEscape {
                        flag: "--work-tree",
                    });
                    return;
                }
            }
        }
    }
}

fn check_rebase_exec(command: &str, lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("rebase") {
        return;
    }
    // Check for --exec flag with a non-empty argument anywhere after "rebase".
    // git rebase --exec 'rm -rf /' branch   → "exec" can be abbreviated to 3+ chars.
    // git rebase -x 'rm -rf /' branch       → short form.
    // git pull --rebase --exec 'rm -rf /'   → --exec forwarded to rebase.
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, &word) in words.iter().enumerate() {
        if word != "git" && !word.ends_with("/git") {
            continue;
        }
        // Direct: git rebase --exec ...
        if let Some(&next) = words.get(i + 1)
            && next == "rebase"
        {
            for &arg in words.iter().skip(i + 2) {
                if arg == "--exec"
                    || arg.starts_with("--exec=")
                    || (arg == "-x" || arg.starts_with("-x"))
                {
                    violations.push(GitSafetyViolation::RebaseExec);
                    return;
                }
                // Stop scanning at subcommand boundaries
                if arg.starts_with('-') && !arg.starts_with("--exec") && arg != "-x" {
                    continue;
                }
            }
            return;
        }
        // Indirect: git pull --rebase --exec ...
        if let Some(&next) = words.get(i + 1)
            && next == "pull"
        {
            let has_rebase_flag = words
                .iter()
                .skip(i + 2)
                .any(|&w| w == "--rebase" || w == "-r" || w.starts_with("--rebase="));
            if has_rebase_flag {
                for &arg in words.iter().skip(i + 2) {
                    if arg == "--exec"
                        || arg.starts_with("--exec=")
                        || (arg == "-x" || arg.starts_with("-x"))
                    {
                        violations.push(GitSafetyViolation::RebaseExec);
                        return;
                    }
                }
            }
            return;
        }
    }
}

fn check_clone_recurse_submodules(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("clone") {
        return;
    }
    if lower.contains("--recurse-submodules") || lower.contains("--recursive") {
        violations.push(GitSafetyViolation::CloneRecurseSubmodules);
    }
}

fn check_submodule_update_init(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("submodule") {
        return;
    }
    // git submodule update --init [--recursive] executes submodule hooks
    let has_update = lower.contains("update");
    let has_init = lower.contains("--init") || lower.contains("--recursive");
    if has_update && has_init {
        violations.push(GitSafetyViolation::SubmoduleUpdateInit);
    }
}

fn check_commit_amend(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if lower.contains("commit") && lower.contains("--amend") {
        violations.push(GitSafetyViolation::CommitAmend);
    }
}

fn check_worktree_destructive(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    let words: Vec<String> = lower
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| matches!(ch, ';' | '(' | ')' | '{' | '}'))
                .to_string()
        })
        .collect();

    for (idx, word) in words.iter().enumerate() {
        if !word_is_git(word) {
            continue;
        }
        let Some(subcommand_idx) = first_git_subcommand(&words, idx + 1) else {
            continue;
        };
        let args_end = command_segment_end(&words, subcommand_idx + 1);
        let args = &words[subcommand_idx + 1..args_end];
        match words[subcommand_idx].as_str() {
            "reset" if args.iter().any(|word| word == "--hard") => {
                violations.push(GitSafetyViolation::WorktreeDestructive {
                    operation: "git reset --hard",
                })
            }
            "restore" if git_restore_touches_worktree(args) => {
                violations.push(GitSafetyViolation::WorktreeDestructive {
                    operation: "git restore",
                })
            }
            "checkout" if git_checkout_discards_worktree(args) => {
                violations.push(GitSafetyViolation::WorktreeDestructive {
                    operation: "git checkout",
                })
            }
            "clean" if git_clean_deletes_worktree(args) => {
                violations.push(GitSafetyViolation::WorktreeDestructive {
                    operation: "git clean",
                })
            }
            _ => {}
        }
    }
}

fn check_branch_force_delete(command: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !command.to_lowercase().contains("branch") {
        return;
    }
    let words: Vec<String> = command
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| matches!(ch, ';' | '(' | ')' | '{' | '}'))
                .to_string()
        })
        .collect();
    for (i, word) in words.iter().enumerate() {
        if !word_is_git(&word.to_lowercase()) {
            continue;
        }
        let Some(sub) = words.get(i + 1) else {
            continue;
        };
        if sub != "branch" {
            continue;
        }

        let args_end = command_segment_end(&words, i + 2);
        let args = &words[i + 2..args_end];
        if args.iter().any(|arg| arg == "-D") {
            violations.push(GitSafetyViolation::BranchForceDelete);
            return;
        }
        let has_delete = args.iter().any(|arg| arg == "-d" || arg == "--delete");
        let has_force = args.iter().any(|arg| arg == "-f" || arg == "--force");
        if has_delete && has_force {
            violations.push(GitSafetyViolation::BranchForceDelete);
            return;
        }
    }
}

fn check_stash_destructive(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("stash") {
        return;
    }
    if lower.contains("stash drop") {
        violations.push(GitSafetyViolation::StashDestructive { operation: "drop" });
        return;
    }
    if lower.contains("stash clear") {
        violations.push(GitSafetyViolation::StashDestructive { operation: "clear" });
    }
}

fn check_tag_delete(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("tag") {
        return;
    }
    let words: Vec<&str> = lower.split_whitespace().collect();
    for (i, &word) in words.iter().enumerate() {
        if !word_is_git(word) {
            continue;
        }
        if let Some(sub) = words.get(i + 1)
            && *sub == "tag"
        {
            for &arg in words.iter().skip(i + 2) {
                if arg == "-d" || arg == "--delete" {
                    violations.push(GitSafetyViolation::TagDelete);
                    return;
                }
            }
        }
    }
}

fn check_bisect_run(lower: &str, violations: &mut Vec<GitSafetyViolation>) {
    if !lower.contains("bisect") {
        return;
    }
    if lower.contains("bisect run") {
        violations.push(GitSafetyViolation::BisectRun);
    }
}

fn word_is_git(word: &str) -> bool {
    word == "git" || word.ends_with("/git")
}

fn is_shell_boundary(word: &str) -> bool {
    matches!(word, "&&" | "||" | "|" | ";")
}

fn command_segment_end(words: &[String], start: usize) -> usize {
    words[start..]
        .iter()
        .position(|word| is_shell_boundary(word))
        .map(|offset| start + offset)
        .unwrap_or(words.len())
}

fn first_git_subcommand(words: &[String], mut idx: usize) -> Option<usize> {
    while idx < words.len() {
        let word = words[idx].as_str();
        if is_shell_boundary(word) {
            return None;
        }
        if matches!(word, "-c" | "--git-dir" | "--work-tree") {
            idx += 2;
            continue;
        }
        if word.starts_with("-c=")
            || word.starts_with("--git-dir=")
            || word.starts_with("--work-tree=")
        {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    None
}

fn short_git_flag_contains(word: &str, flag: char) -> bool {
    word.starts_with('-') && !word.starts_with("--") && word.chars().skip(1).any(|ch| ch == flag)
}

fn args_request_help(args: &[String]) -> bool {
    args.iter()
        .any(|word| matches!(word.as_str(), "-h" | "--help" | "help"))
}

fn git_restore_touches_worktree(args: &[String]) -> bool {
    if args_request_help(args) {
        return false;
    }
    let has_staged = args
        .iter()
        .any(|word| word == "--staged" || short_git_flag_contains(word, 'S'));
    let has_worktree = args
        .iter()
        .any(|word| word == "--worktree" || short_git_flag_contains(word, 'W'));
    has_worktree || !has_staged
}

fn git_checkout_discards_worktree(args: &[String]) -> bool {
    if args_request_help(args) {
        return false;
    }
    if args.iter().any(|word| {
        word == "--force"
            || word == "-f"
            || short_git_flag_contains(word, 'f')
            || short_git_flag_contains(word, 'F')
    }) {
        return true;
    }
    args.windows(2)
        .any(|window| window[0] == "--" && !window[1].is_empty())
}

fn git_clean_deletes_worktree(args: &[String]) -> bool {
    if args_request_help(args) {
        return false;
    }
    let dry_run = args
        .iter()
        .any(|word| word == "--dry-run" || word == "-n" || short_git_flag_contains(word, 'n'));
    if dry_run {
        return false;
    }
    args.iter()
        .any(|word| word == "--force" || word == "-f" || short_git_flag_contains(word, 'f'))
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
            r#"git commit --message="$(whoami) was here""#,
            r#"git commit -m"$(whoami) was here""#,
            r#"git commit -m$(whoami)"#,
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

    // --- Blocked operations ---

    #[test]
    fn blocked_git_operations() {
        // --no-verify
        let v = validate_git_command("git commit --no-verify -m 'skip hooks'");
        assert!(v.iter().any(|v| matches!(
            v,
            GitSafetyViolation::HookSkipFlag {
                flag: "--no-verify"
            }
        )));

        // --amend
        let v = validate_git_command("git commit --amend -m 'rewrite'");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CommitAmend))
        );
    }

    #[test]
    fn worktree_destructive_operations_require_approval() {
        for (cmd, operation) in [
            ("git reset --hard HEAD~1", "git reset --hard"),
            (
                "git restore --staged --worktree rust/crates/foo/src/lib.rs",
                "git restore",
            ),
            ("git restore -- .", "git restore"),
            ("git checkout -- .", "git checkout"),
            ("git checkout -f main", "git checkout"),
            ("git clean -fd", "git clean"),
            ("git stash && git checkout origin/main -- .", "git checkout"),
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter().any(|v| matches!(
                    v,
                    GitSafetyViolation::WorktreeDestructive { operation: op } if *op == operation
                )),
                "should flag {operation}: {cmd}; got {v:?}"
            );
        }
    }

    #[test]
    fn non_destructive_worktree_queries_remain_allowed() {
        for cmd in [
            "git status --short",
            "git restore --staged rust/crates/foo/src/lib.rs",
            "git checkout main",
            "git clean -nfd",
            "git restore --help",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                !v.iter()
                    .any(|v| matches!(v, GitSafetyViolation::WorktreeDestructive { .. })),
                "false positive for {cmd}: {v:?}"
            );
        }
    }

    #[test]
    fn force_push_behavior() {
        // Detect force push
        for (cmd, is_protected) in [
            ("git push --force origin main", true),
            ("git push -f origin main", false),
            ("git push --force origin master", true),
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::ForcePush)),
                "ForcePush for: {cmd}"
            );
            if is_protected {
                assert!(
                    v.iter().any(|violation| matches!(
                        violation,
                        GitSafetyViolation::ForcePushProtectedBranch { .. }
                    )),
                    "ForcePushProtectedBranch for: {cmd}"
                );
            }
        }
        // Feature branch NOT protected
        let v = validate_git_command("git push --force origin feature/my-feature");
        assert!(!v.iter().any(|violation| matches!(
            violation,
            GitSafetyViolation::ForcePushProtectedBranch { .. }
        )));
        // Feature branches containing "main"/"develop" are NOT protected (false positive regression)
        for cmd in [
            "git push --force origin feature/main-refactor",
            "git push -f origin feature/develop-ui",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::ForcePush))
            );
            assert!(
                !v.iter().any(|violation| matches!(
                    violation,
                    GitSafetyViolation::ForcePushProtectedBranch { .. }
                )),
                "false positive for {cmd}"
            );
        }
        // "origin/main" with remote prefix IS protected
        let v = validate_git_command("git push --force origin origin/main");
        assert!(v.iter().any(|violation| matches!(
            violation,
            GitSafetyViolation::ForcePushProtectedBranch { .. }
        )));
        // Non-force flags are NOT force push
        for cmd in [
            "git push --follow-tags origin my-branch",
            "git push -ff origin my-branch",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                !v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::ForcePush)),
                "false positive: {cmd}"
            );
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
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CdGitCompound))
        );
        let v = validate_git_command("cd /tmp/evil&&git status");
        assert!(
            v.iter()
                .any(|v| matches!(v, GitSafetyViolation::CdGitCompound))
        );
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
            "git --work-tree=/tmp/evil status",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter().any(|violation| matches!(
                    violation,
                    GitSafetyViolation::GitConfigFlag
                        | GitSafetyViolation::GitExecPathFlag
                        | GitSafetyViolation::GitBoundaryEscape { .. }
                )),
                "should block: {cmd}"
            );
        }
    }

    #[test]
    fn branch_force_delete_detected_without_lowercasing_flag() {
        for cmd in [
            "git branch -D old-feature",
            "git branch --delete --force old-feature",
            "git branch -d --force old-feature",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::BranchForceDelete)),
                "should flag branch force delete: {cmd}; got {v:?}"
            );
        }

        let v = validate_git_command("git branch -d old-feature");
        assert!(
            !v.iter()
                .any(|violation| matches!(violation, GitSafetyViolation::BranchForceDelete)),
            "safe branch delete should not be force delete: {v:?}"
        );
    }

    #[test]
    fn rebase_exec_blocked() {
        for cmd in [
            "git rebase --exec 'rm -rf /' main",
            "git rebase -x 'rm -rf /' main",
            "git rebase --exec='rm -rf /' main",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::RebaseExec)),
                "should block rebase --exec: {cmd}"
            );
        }
        // Plain rebase without --exec is allowed
        let v = validate_git_command("git rebase main");
        assert!(
            !v.iter()
                .any(|violation| matches!(violation, GitSafetyViolation::RebaseExec)),
            "should allow plain rebase"
        );
    }

    #[test]
    fn clone_recurse_submodules_blocked() {
        for cmd in [
            "git clone --recurse-submodules https://evil/repo",
            "git clone --recursive https://evil/repo",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter().any(|violation| matches!(
                    violation,
                    GitSafetyViolation::CloneRecurseSubmodules
                )),
                "should block: {cmd}"
            );
        }
        // Plain clone is allowed
        let v = validate_git_command("git clone https://safe/repo");
        assert!(
            !v.iter()
                .any(|violation| matches!(violation, GitSafetyViolation::CloneRecurseSubmodules))
        );
    }

    #[test]
    fn submodule_update_init_blocked() {
        for cmd in [
            "git submodule update --init",
            "git submodule update --init --recursive",
            "git submodule update --recursive",
        ] {
            let v = validate_git_command(cmd);
            assert!(
                v.iter()
                    .any(|violation| matches!(violation, GitSafetyViolation::SubmoduleUpdateInit)),
                "should block: {cmd}"
            );
        }
        // Plain submodule status is allowed
        let v = validate_git_command("git submodule status");
        assert!(
            !v.iter()
                .any(|violation| matches!(violation, GitSafetyViolation::SubmoduleUpdateInit))
        );
    }

    // --- validate_git_command edge cases ---

    #[test]
    fn validate_git_command_edge_cases() {
        // Non-git commands
        for cmd in ["echo hello", ""] {
            let violations = validate_git_command(cmd);
            assert!(violations.is_empty(), "should be empty for: {cmd}");
        }
        // Multiple violations in one command
        let violations = validate_git_command("git commit --amend --no-verify");
        assert!(
            violations.len() >= 2,
            "should detect both amend and no-verify"
        );
    }

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

    // --- violation display ---
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
            GitSafetyViolation::WorktreeDestructive {
                operation: "git reset --hard",
            },
            GitSafetyViolation::BareRepoDetected,
            GitSafetyViolation::RebaseExec,
            GitSafetyViolation::CloneRecurseSubmodules,
            GitSafetyViolation::SubmoduleUpdateInit,
        ];
        for v in &violations {
            let msg = format!("{v}");
            assert!(!msg.is_empty());
        }
    }
}
