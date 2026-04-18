//! Which edge tool names require an `approval_required` round-trip before `tool_request`
//! in cloud-orchestrated delivery ([`super::cloud_tool_delivery`]).
//!
//! CLI permission prompts use [`cloud_gated_tool_kind`] so icons (Execute vs Write) and cloud gating
//! cannot drift.

/// Canonical tool names that must pass user approval (thin-client ledger) before edge execution.
pub const CLOUD_APPROVAL_REQUIRED_TOOLS: &[&str] = &[
    "bash",
    "create_file",
    "delete_file",
    "edit_file",
    "exec",
    "git_stash",
    "github_create_issue",
    "multi_edit",
    "rollback_database_snapshots",
    "rollback_file_edits",
    "rollback_turn_actions",
    "run_command",
    "shell",
    "str_replace",
    "write_file",
];

/// Subset of [`CLOUD_APPROVAL_REQUIRED_TOOLS`] that take a shell `command` argument (CLI ▶).
pub const CLOUD_APPROVAL_EXECUTE_TOOLS: &[&str] = &["bash", "exec", "run_command", "shell"];

/// Read-only shell commands that can run concurrently without user approval.
/// These commands only read data and don't modify system state.
const READ_ONLY_COMMANDS: &[&str] = &[
    // File viewing
    "cat",
    "head",
    "tail",
    "wc",
    "stat",
    "ls",
    "ll",
    "tree",
    "file",
    // Git read-only
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    "git ls-files",
    "git rev-parse",
    "git describe",
    "git remote",
    "git config --get",
    "git config --list",
    "git tag",
    "git stash list",
    // Search tools
    "grep",
    "find",
    "fd",
    "rg",
    "ag",
    "ack",
    "locate",
    // System info
    "pwd",
    "which",
    "type",
    "uname",
    "id",
    "df",
    "du",
    "free",
    "uptime",
    "whoami",
    "hostname",
    "env",
    "printenv",
    "date",
    "cal",
    "nproc",
    // Text processing (no output redirection)
    "cut",
    "paste",
    "tr",
    "sort",
    "uniq",
    "nl",
    "column",
    "fmt",
    "fold",
    "expand",
    // Path tools
    "basename",
    "dirname",
    "realpath",
    "readlink",
    // Misc safe
    "echo",
    "printf",
    "true",
    "false",
    "test",
    "expr",
    "seq",
    "sleep",
    // Rust/Cargo read-only
    "cargo check",
    "cargo clippy",
    "cargo fmt --check",
    "cargo test --no-run",
    "rustfmt --check",
    // Node/npm read-only
    "npm list",
    "npm ls",
    "npm outdated",
    "npm audit",
    "node --version",
    "npm --version",
    // Python read-only
    "python --version",
    "python3 --version",
    "pip list",
    "pip3 list",
    "pip freeze",
    "pip3 freeze",
];

/// Patterns that indicate a command has side effects (not read-only).
const WRITE_INDICATORS: &[&str] = &[
    // Output redirection
    ">",
    ">>",
    // Pipe to potentially dangerous commands
    "| tee ",
    "| xargs ",
    "| sh",
    "| bash",
    "| sudo",
    // Git write operations
    "git add",
    "git commit",
    "git push",
    "git pull",
    "git merge",
    "git rebase",
    "git reset",
    "git checkout",
    "git stash pop",
    "git stash apply",
    "git stash drop",
    "git stash clear",
    "git clean",
    "git rm",
    "git mv",
    // File operations
    "rm ",
    "mv ",
    "cp ",
    "mkdir ",
    "rmdir ",
    "touch ",
    "chmod ",
    "chown ",
    "ln ",
    // Package managers (install/modify)
    "npm install",
    "npm i ",
    "npm uninstall",
    "npm update",
    "pip install",
    "pip3 install",
    "pip uninstall",
    "cargo install",
    "cargo build",
    "cargo run",
    "cargo clean",
    "apt install",
    "apt-get install",
    "brew install",
    // Dangerous
    "sudo ",
    "su ",
    "eval ",
    "exec ",
];

fn effective_bash_command(command: &str) -> &str {
    let cmd = command.trim();
    if cmd.starts_with("cd ") && cmd.contains("&&") {
        cmd.split("&&").nth(1).map(str::trim).unwrap_or(cmd)
    } else {
        cmd
    }
}

fn strip_benign_fd_redirects(command: &str) -> String {
    command
        .replace("2>&1", " ")
        .replace("1>&2", " ")
        .replace("2>/dev/null", " ")
        .replace("1>/dev/null", " ")
        .replace(">/dev/null", " ")
}

fn has_write_indicators(command: &str) -> bool {
    WRITE_INDICATORS
        .iter()
        .any(|indicator| command.contains(indicator))
}

fn matches_read_only_prefix(command: &str) -> bool {
    for ro_cmd in READ_ONLY_COMMANDS {
        if let Some(rest) = command.strip_prefix(ro_cmd) {
            // Ensure it's a word boundary (not a prefix of a longer command)
            if rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\n') {
                return true;
            }
        }
    }
    false
}

fn bash_segment_is_read_only(command: &str) -> bool {
    let cmd = command.trim();
    !cmd.is_empty() && !has_write_indicators(cmd) && matches_read_only_prefix(cmd)
}

/// Check if a bash command is read-only (safe for concurrent execution).
///
/// Returns `true` if the command appears to only read data without side effects.
/// Used to allow read-only bash commands to run concurrently without user approval.
///
/// # Algorithm
/// 1. Normalize harmless fd forwarding (`2>&1`, `1>&2`, `/dev/null`)
/// 2. Split read-only pipelines (`cargo check | head -50`) into segments
/// 3. Reject any segment with write indicators; otherwise match read-only prefixes
/// 4. Default to false (require approval) for unknown commands
pub fn bash_command_is_read_only(command: &str) -> bool {
    let cmd = effective_bash_command(command);

    // Empty command is not read-only (edge case)
    if cmd.is_empty() {
        return false;
    }

    let normalized = strip_benign_fd_redirects(cmd);
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return false;
    }

    if normalized.contains('|') {
        let segments: Vec<&str> = normalized
            .split('|')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect();
        return !segments.is_empty()
            && segments
                .iter()
                .all(|segment| bash_segment_is_read_only(segment));
    }

    bash_segment_is_read_only(normalized)
}

/// Kind of side effect for tools gated before edge execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudGatedToolKind {
    Write,
    Execute,
}

/// Returns [`None`] when the tool is not cloud-gated (treated as read-only for approval purposes).
#[inline]
pub fn cloud_gated_tool_kind(name: &str) -> Option<CloudGatedToolKind> {
    // MCP tools run external server code with unknown side effects —
    // treat them as Execute (highest-risk) for permission gating.
    if name.starts_with("mcp_") {
        return Some(CloudGatedToolKind::Execute);
    }
    if !CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name) {
        return None;
    }
    if CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name) {
        Some(CloudGatedToolKind::Execute)
    } else {
        Some(CloudGatedToolKind::Write)
    }
}

/// Returns true if `name` is in [`CLOUD_APPROVAL_REQUIRED_TOOLS`].
#[inline]
pub fn edge_tool_requires_cloud_approval(name: &str) -> bool {
    cloud_gated_tool_kind(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_listed_tool_requires_approval() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS {
            assert!(
                edge_tool_requires_cloud_approval(name),
                "list entry must satisfy predicate: {name}"
            );
        }
    }

    #[test]
    fn read_only_tools_skip_approval_gate() {
        for name in ["read_file", "list_dir", "grep", "glob", "git_status"] {
            assert!(
                !edge_tool_requires_cloud_approval(name),
                "{name} should not require cloud approval"
            );
        }
    }

    #[test]
    fn unknown_tool_not_gated() {
        assert!(!edge_tool_requires_cloud_approval("made_up_tool"));
        assert!(!edge_tool_requires_cloud_approval(""));
    }

    #[test]
    fn list_is_sorted_for_stable_diffs() {
        let mut sorted = CLOUD_APPROVAL_REQUIRED_TOOLS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            CLOUD_APPROVAL_REQUIRED_TOOLS,
            sorted.as_slice(),
            "CLOUD_APPROVAL_REQUIRED_TOOLS should stay sorted"
        );
    }

    #[test]
    fn execute_tools_sorted_and_subset_of_required() {
        let mut sorted = CLOUD_APPROVAL_EXECUTE_TOOLS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            CLOUD_APPROVAL_EXECUTE_TOOLS,
            sorted.as_slice(),
            "CLOUD_APPROVAL_EXECUTE_TOOLS should stay sorted"
        );
        for &name in CLOUD_APPROVAL_EXECUTE_TOOLS {
            assert!(
                CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name),
                "{name} must appear in CLOUD_APPROVAL_REQUIRED_TOOLS"
            );
        }
    }

    #[test]
    fn required_tools_partition_into_execute_and_write() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS {
            let kind = cloud_gated_tool_kind(name).expect("required tools must classify");
            match kind {
                CloudGatedToolKind::Execute => {
                    assert!(CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name));
                }
                CloudGatedToolKind::Write => {
                    assert!(!CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name));
                }
            }
        }
    }

    #[test]
    fn git_stash_is_write_gated() {
        assert_eq!(
            cloud_gated_tool_kind("git_stash"),
            Some(CloudGatedToolKind::Write)
        );
    }

    #[test]
    fn delete_file_is_write_gated() {
        assert_eq!(
            cloud_gated_tool_kind("delete_file"),
            Some(CloudGatedToolKind::Write)
        );
    }

    #[test]
    fn github_create_issue_is_write_gated() {
        assert_eq!(
            cloud_gated_tool_kind("github_create_issue"),
            Some(CloudGatedToolKind::Write)
        );
    }

    // ── bash_command_is_read_only tests ──

    #[test]
    fn bash_read_only_commands() {
        // Simple read-only commands
        assert!(bash_command_is_read_only("ls"));
        assert!(bash_command_is_read_only("ls -la"));
        assert!(bash_command_is_read_only("cat file.txt"));
        assert!(bash_command_is_read_only("head -n 10 file.txt"));
        assert!(bash_command_is_read_only("tail -f log.txt"));
        assert!(bash_command_is_read_only("wc -l file.txt"));
        assert!(bash_command_is_read_only("pwd"));
        assert!(bash_command_is_read_only("whoami"));
        assert!(bash_command_is_read_only("date"));

        // Git read-only
        assert!(bash_command_is_read_only("git status"));
        assert!(bash_command_is_read_only("git log --oneline"));
        assert!(bash_command_is_read_only("git diff HEAD"));
        assert!(bash_command_is_read_only("git show abc123"));
        assert!(bash_command_is_read_only("git branch -a"));
        assert!(bash_command_is_read_only("git ls-files"));

        // Search tools
        assert!(bash_command_is_read_only("grep -r pattern ."));
        assert!(bash_command_is_read_only("find . -name '*.rs'"));
        assert!(bash_command_is_read_only("rg pattern"));

        // Cargo/npm read-only
        assert!(bash_command_is_read_only("cargo check"));
        assert!(bash_command_is_read_only("cargo clippy"));
        assert!(bash_command_is_read_only("npm list"));
        assert!(bash_command_is_read_only("cargo check 2>&1 | head -50"));
        assert!(bash_command_is_read_only(
            "cd rust && cargo test --no-run 2>&1 | tail -20"
        ));

        // cd-prefixed commands
        assert!(bash_command_is_read_only("cd project && ls"));
        assert!(bash_command_is_read_only("cd /tmp && cat file.txt"));
    }

    #[test]
    fn bash_write_commands_not_read_only() {
        // File operations
        assert!(!bash_command_is_read_only("rm file.txt"));
        assert!(!bash_command_is_read_only("mv a.txt b.txt"));
        assert!(!bash_command_is_read_only("cp a.txt b.txt"));
        assert!(!bash_command_is_read_only("mkdir dir"));
        assert!(!bash_command_is_read_only("touch file.txt"));

        // Git write operations
        assert!(!bash_command_is_read_only("git add ."));
        assert!(!bash_command_is_read_only("git commit -m 'msg'"));
        assert!(!bash_command_is_read_only("git push origin main"));
        assert!(!bash_command_is_read_only("git checkout main"));
        assert!(!bash_command_is_read_only("git reset --hard"));

        // Output redirection
        assert!(!bash_command_is_read_only("ls > output.txt"));
        assert!(!bash_command_is_read_only("echo hello >> file.txt"));

        // Package installation
        assert!(!bash_command_is_read_only("npm install package"));
        assert!(!bash_command_is_read_only("pip install package"));
        assert!(!bash_command_is_read_only("cargo build"));

        // Dangerous commands
        assert!(!bash_command_is_read_only("sudo rm -rf /"));
        assert!(!bash_command_is_read_only("eval 'echo bad'"));
    }

    #[test]
    fn bash_pipe_to_dangerous_commands() {
        assert!(!bash_command_is_read_only("ls | tee output.txt"));
        assert!(!bash_command_is_read_only("echo test | xargs rm"));
        assert!(!bash_command_is_read_only("cat script.sh | bash"));
        assert!(!bash_command_is_read_only(
            "cargo check 2>&1 | tee build.log"
        ));
    }

    #[test]
    fn bash_empty_command() {
        assert!(!bash_command_is_read_only(""));
        assert!(!bash_command_is_read_only("   "));
    }

    #[test]
    fn bash_unknown_commands_not_read_only() {
        // Unknown commands should require approval (conservative)
        assert!(!bash_command_is_read_only("custom_script.sh"));
        assert!(!bash_command_is_read_only("./run.sh"));
        assert!(!bash_command_is_read_only("make"));
        assert!(!bash_command_is_read_only("docker run image"));
    }

    // ── MCP permission gating tests ──

    #[test]
    fn mcp_tools_require_approval() {
        assert!(edge_tool_requires_cloud_approval("mcp_filesystem_read"));
        assert!(edge_tool_requires_cloud_approval("mcp_github_search"));
        assert!(edge_tool_requires_cloud_approval(
            "mcp_custom_server_do_stuff"
        ));
    }

    #[test]
    fn mcp_tools_classified_as_execute() {
        assert_eq!(
            cloud_gated_tool_kind("mcp_anything"),
            Some(CloudGatedToolKind::Execute),
        );
        assert_eq!(
            cloud_gated_tool_kind("mcp_server_tool"),
            Some(CloudGatedToolKind::Execute),
        );
    }

    #[test]
    fn non_mcp_unknown_tool_not_gated() {
        // "mcp" without underscore prefix should NOT match
        assert!(!edge_tool_requires_cloud_approval("mcp"));
        assert!(!edge_tool_requires_cloud_approval("my_mcp_tool"));
    }
}
