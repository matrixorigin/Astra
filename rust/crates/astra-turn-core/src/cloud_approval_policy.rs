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
    "git_commit",
    "git_revert_commit",
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

fn effective_bash_command(command: &str) -> &str {
    command.trim()
}

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
        // Look for an fd-redirect operator at a left token boundary:
        //   * `<digit>>`  / `<digit>>>`           — e.g. `2>foo`, `2>>foo`
        //   * `&>` / `&>>` (bash combined redir)  — e.g. `&>foo`, `&>>foo`
        //
        // Left-boundary check guards against digit-in-argument cases like
        // `echo a2>foo` where `a2` is echo's arg and `>foo` is a real
        // stdout redirect — pinned by `fd_redirect_requires_left_token_boundary`.
        let left_ok = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'\n' | b'|' | b';' | b'&' | b'('
            );
        let op_len: Option<usize> =
            if left_ok && b.is_ascii_digit() && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                // `N>` or `N>>`
                if i + 2 < bytes.len() && bytes[i + 2] == b'>' {
                    Some(3)
                } else {
                    Some(2)
                }
            } else if left_ok && b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                // `&>` or `&>>`
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
            // Skip spaces/tabs between operator and filename.
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            // Consume the filename token (non-whitespace, non-pipe,
            // non-semicolon, non-`&`). `&` is in the stop set because real
            // redirect targets can't start with `&` in shell grammar — fd
            // duplication uses `>&N` (no space), not `> &N`, so a lone `&`
            // always marks the next command token or background operator.
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
            // Malformed tail (e.g. `cmd 2>` / `cmd &>` with no target):
            // fall through to the UTF-8 copy path below. The operator bytes
            // (digit/`&` and one or two `>`) are preserved verbatim across
            // successive iterations so downstream `>` / `>>` scans see the
            // original dangling redirect and conservatively classify as
            // mutating. Contract pinned by
            // `malformed_trailing_redirect_stays_conservative`.
        }
        // Copy the next UTF-8 scalar verbatim. Using `char_indices`-style
        // advancement keeps non-ASCII filenames (e.g. Chinese paths) intact
        // instead of producing mojibake via `b as char` on a continuation
        // byte.
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
    // Deliberately deny-by-default at string level. This rejects harmless quoted
    // literals such as `grep ';' file`, but keeps approval classification from
    // needing to prove shell quoting correctness.
    command.contains("$(") || command.contains('`') || command.contains(';')
}

fn split_compound_segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split('\n')
        .flat_map(|chunk| chunk.split("&&"))
        .flat_map(|chunk| chunk.split("||"))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
}

/// Why a bash command was classified as **requiring approval** (not read-only).
///
/// Returned by [`bash_command_approval_reason`]. Surfaced in CLI approval
/// prompts so users can understand *why* a command tripped the classifier
/// (e.g. "writes to file via `>`", "shell injection vector `$(`", …) rather
/// than just seeing a generic "approval required" banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BashApprovalReason {
    /// Command string was empty after trimming.
    Empty,
    /// Contains `$(`, backtick, or `;` — can hide arbitrary commands.
    ShellInjection,
    /// Contains a write indicator (`>`, `rm`, `sed -i`, etc.). `String` is
    /// the first matched indicator for display.
    WriteIndicator(String),
    /// Command prefix is not in the read-only allowlist. `String` is the
    /// first token (e.g. `foobar`, `npm`) for display.
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
                // Action-oriented phrasing tells the user *what the command
                // does*; the raw token is cited in parentheses so power users
                // can still correlate with their command text.
                let action = humanize_write_indicator(ind.as_str());
                format!("{action} (`{trimmed}`)", trimmed = ind.trim())
            }
            BashApprovalReason::UnknownPrefix(tok) => {
                // Frame as risk ("may modify your system") rather than
                // implementation detail ("not in allowlist") — the latter is
                // jargon for non-developer users.
                format!("`{tok}` may modify your system (unrecognized command)")
            }
        }
    }
}

/// Map a [`WRITE_INDICATORS`] token to an action-oriented phrase suitable
/// for end-user approval prompts. Falls back to a generic phrase when no
/// specific mapping exists (so new indicators added to `WRITE_INDICATORS`
/// don't silently regress to machine-translation-y default text).
///
/// Kept as a small, explicit table rather than a HashMap: the indicator list
/// is static and short, ordering doesn't matter, and a table is grep-friendly
/// for future maintainers.
fn humanize_write_indicator(indicator: &str) -> &'static str {
    // Prefix-match table ordered most-specific-first so `"sed -i"` wins over
    // a hypothetical shorter `"sed"`. We match by `starts_with` (after trim)
    // because some indicators carry a trailing space (e.g. `"rm "`).
    let trimmed = indicator.trim();
    const TABLE: &[(&str, &str)] = &[
        // Redirections — most common case; glyph is opaque to non-technical users.
        (">>", "appends to a file"),
        (">", "writes to a file"),
        // File operations
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
        // Git write verbs
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
        // Package managers
        ("npm install", "installs packages"),
        ("npm i", "installs packages"),
        ("npm uninstall", "uninstalls packages"),
        ("npm update", "updates packages"),
        ("pip install", "installs packages"),
        ("pip3 install", "installs packages"),
        ("pip uninstall", "uninstalls packages"),
        ("cargo install", "installs a cargo binary"),
        ("cargo build", "builds the cargo project"),
        // Pipes that can execute arbitrary code
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
    // Conservative fallback: new indicators added to WRITE_INDICATORS without
    // a humanized mapping still get a non-jargon phrase. The raw token is
    // appended by the caller so users see what tripped.
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
    let cmd = effective_bash_command(command);

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

    let mut saw_segment = false;
    let mut first_failure: Option<BashApprovalReason> = None;
    for segment in split_compound_segments(normalized) {
        saw_segment = true;
        let reason = if segment.contains('|') {
            let mut saw_pipe_segment = false;
            let mut pipe_failure: Option<BashApprovalReason> = None;
            for pipe_segment in segment.split('|').map(str::trim).filter(|s| !s.is_empty()) {
                saw_pipe_segment = true;
                if let Some(r) = bash_segment_approval_reason(pipe_segment) {
                    pipe_failure = Some(r);
                    break;
                }
            }
            if !saw_pipe_segment {
                Some(BashApprovalReason::Empty)
            } else {
                pipe_failure
            }
        } else {
            bash_segment_approval_reason(segment)
        };
        if let Some(r) = reason {
            first_failure = Some(r);
            break;
        }
    }
    if !saw_segment {
        return Some(BashApprovalReason::Empty);
    }
    first_failure
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
    fn git_commit_is_write_gated() {
        assert_eq!(
            cloud_gated_tool_kind("git_commit"),
            Some(CloudGatedToolKind::Write)
        );
    }

    #[test]
    fn git_revert_commit_is_write_gated() {
        assert_eq!(
            cloud_gated_tool_kind("git_revert_commit"),
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

    /// Regression: every benign fd redirect pattern in
    /// `strip_benign_fd_redirects` must keep the command read-only. Twin of
    /// `astra_runtime::bash_intent::benign_fd_redirects_are_not_mutating`;
    /// keep the two corpora aligned.
    #[test]
    fn benign_fd_redirects_remain_read_only() {
        assert!(bash_command_is_read_only("cargo check 2>&1"));
        assert!(bash_command_is_read_only("cargo check 1>&2"));
        assert!(bash_command_is_read_only("cargo check 2>/dev/null"));
        assert!(bash_command_is_read_only("cargo check 1>/dev/null"));
        assert!(bash_command_is_read_only("cargo check >/dev/null"));
        assert!(bash_command_is_read_only("cargo check &>/dev/null"));
        // Append-form combined redirect: `&>>` must also be stripped so the
        // downstream `>>` scan doesn't flag a pure-logging append as mutation.
        assert!(bash_command_is_read_only("cargo check &>> /tmp/unused_log"));
        assert!(bash_command_is_read_only("cargo check 2>&1 | head -50"));
    }

    /// Residual-risk guard: after `strip_benign_fd_redirects` removes the
    /// redirect operator, the surviving target-filename token MUST NOT
    /// accidentally match any [`WRITE_INDICATORS`] entry. This invariant holds
    /// today because every file-op indicator carries a trailing space (`"rm "`,
    /// `"mv "`, …) and shell redirect targets are single whitespace-free
    /// tokens — but it is load-bearing and silently brittle, so we pin it as
    /// an adversarial corpus. Twin of
    /// `astra_runtime::bash_intent::benign_fd_redirect_target_filenames_are_inert`.
    #[test]
    fn benign_fd_redirect_target_filenames_are_inert() {
        // Filenames that textually contain write-indicator substrings.
        assert!(bash_command_is_read_only("cargo check &>> /tmp/rm_me.log"));
        assert!(bash_command_is_read_only(
            "cargo check &>> /var/log/mv_state"
        ));
        assert!(bash_command_is_read_only("cargo check &>> ./cp_backup.log"));
        assert!(bash_command_is_read_only("cargo check &> /tmp/chmod.out"));
        assert!(bash_command_is_read_only(
            "cargo check 2> /tmp/git_commit_trace.log"
        ));
        // Combined with a pipe-to-reader: still pure read-only plumbing.
        assert!(bash_command_is_read_only(
            "cargo check &>> /tmp/rm_me.log && echo done"
        ));
        // Malformed tail (no target after `2>`): must not panic and must not
        // accidentally strip surrounding bytes. Shell itself would error on
        // a dangling redirect, so we *conservatively* let the surviving `>`
        // trip the mutation scan — better a false positive on a malformed
        // command than a silent miss. Pinned here so future "smarter"
        // stripping must consciously change this contract.
        assert!(!bash_command_is_read_only("cargo check 2>"));
        // Non-ASCII filename (Chinese path). The byte-level scanner must
        // preserve the UTF-8 sequence verbatim rather than emit mojibake
        // that could randomly hit a write-indicator substring.
        assert!(bash_command_is_read_only("cargo check &>> /tmp/日志.log"));
    }

    /// Residual-risk guard: fd-redirect detection MUST require a token
    /// boundary to the **left** of the digit. Without this, a digit that is
    /// actually part of an argument (`echo a2>/tmp/x` — `a2` is the echo
    /// argument, `>` is the real stdout redirect writing to `/tmp/x`) would
    /// be over-stripped and the command would silently read as read-only,
    /// hiding a genuine workspace mutation. Twin of
    /// `astra_runtime::bash_intent::fd_redirect_requires_left_token_boundary`.
    #[test]
    fn fd_redirect_requires_left_token_boundary() {
        // `a2>/tmp/x` is `echo`'s argument `a2` plus a real stdout redirect
        // — the command writes to `/tmp/x`, so it MUST NOT be read-only.
        assert!(!bash_command_is_read_only("echo a2>/tmp/x"));
        // Same with append form.
        assert!(!bash_command_is_read_only("echo a2>>/tmp/x"));
        // Sanity: a genuine fd redirect (digit at a token boundary) is still
        // stripped correctly.
        assert!(bash_command_is_read_only("cargo check 2>/tmp/log"));
        // Start-of-string digit is a genuine fd specifier.
        assert!(bash_command_is_read_only("2>/tmp/log cargo check"));
        // After a pipe / semicolon / `&` / `(` the digit is also a genuine
        // fd specifier (fresh command token boundary).
        assert!(bash_command_is_read_only("true | 2>/tmp/log cargo check"));
    }

    // ── bash_command_approval_reason tests (TDD for rationale surfacing) ──

    /// Read-only commands must return `None` (no approval reason) — the
    /// reason API must stay in lock-step with `bash_command_is_read_only`.
    #[test]
    fn approval_reason_none_for_read_only() {
        assert_eq!(bash_command_approval_reason("ls -la"), None);
        assert_eq!(
            bash_command_approval_reason("cargo check 2>&1 | head"),
            None
        );
        assert_eq!(bash_command_approval_reason("git status"), None);
    }

    /// Empty / whitespace-only commands must surface [`BashApprovalReason::Empty`].
    #[test]
    fn approval_reason_empty_command() {
        assert_eq!(
            bash_command_approval_reason(""),
            Some(BashApprovalReason::Empty)
        );
        assert_eq!(
            bash_command_approval_reason("   "),
            Some(BashApprovalReason::Empty)
        );
    }

    /// Shell injection vectors (`$(`, backtick, `;`) must surface
    /// [`BashApprovalReason::ShellInjection`] so the approval prompt can
    /// explain that arbitrary commands could be hidden.
    #[test]
    fn approval_reason_shell_injection() {
        assert_eq!(
            bash_command_approval_reason("echo $(rm -rf /)"),
            Some(BashApprovalReason::ShellInjection)
        );
        assert_eq!(
            bash_command_approval_reason("echo `rm -rf /`"),
            Some(BashApprovalReason::ShellInjection)
        );
        assert_eq!(
            bash_command_approval_reason("ls; rm foo"),
            Some(BashApprovalReason::ShellInjection)
        );
    }

    /// Write indicators must be surfaced with the matched token so the
    /// approval prompt can display *which* mutation pattern tripped.
    /// Post-humanization contract: `display()` cites the raw token (so
    /// power users can correlate) and describes the action in plain
    /// language. See `approval_reason_write_indicator_display_is_humanized`
    /// for the full per-indicator phrase contract.
    #[test]
    fn approval_reason_write_indicator_names_the_token() {
        let reason = bash_command_approval_reason("rm -rf /tmp/foo");
        match reason {
            Some(BashApprovalReason::WriteIndicator(ref ind)) => {
                assert!(
                    !ind.is_empty(),
                    "write indicator must carry the matched token for display"
                );
                assert!(
                    ind.trim() == "rm" || ind.starts_with("rm"),
                    "expected `rm` indicator, got {ind:?}"
                );
            }
            other => panic!("expected WriteIndicator, got {other:?}"),
        }
        // Verify `display()` cites the raw token (trimmed) so power users
        // can correlate with their command text.
        let display = reason.unwrap().display();
        assert!(
            display.contains("rm"),
            "display must cite raw token `rm`: {display}"
        );
        // And it uses humanized action-oriented prose (not jargon).
        assert!(
            !display.contains("write indicator"),
            "display must not leak `write indicator` jargon: {display}"
        );
    }

    /// Unknown-prefix commands must name the first token so users can see
    /// *which* command failed the allowlist check.
    #[test]
    fn approval_reason_unknown_prefix_names_first_token() {
        assert_eq!(
            bash_command_approval_reason("foobar --flag"),
            Some(BashApprovalReason::UnknownPrefix("foobar".to_string()))
        );
        // Pipeline: first-failing pipe segment's first token is reported.
        match bash_command_approval_reason("cat file | foobar") {
            Some(BashApprovalReason::UnknownPrefix(tok)) => {
                assert_eq!(tok, "foobar");
            }
            other => panic!("expected UnknownPrefix(foobar), got {other:?}"),
        }
    }

    /// UX: `WriteIndicator::display()` must use action-oriented prose that
    /// explains *what the command does* rather than leaking the raw token
    /// name ("write indicator `>` detected" is machine-translation-y and the
    /// `>` glyph is opaque to non-technical users). Verify humanized
    /// mappings exist for the most common indicators.
    #[test]
    fn approval_reason_write_indicator_display_is_humanized() {
        let cases = [
            (">", "writes to a file"),
            (">>", "appends to a file"),
            ("rm ", "deletes files"),
            ("mv ", "moves or renames files"),
            ("sed -i", "edits files in place"),
            ("chmod ", "changes file permissions"),
            ("npm install", "installs packages"),
        ];
        for (ind, expected_phrase) in cases {
            let display = BashApprovalReason::WriteIndicator(ind.to_string()).display();
            assert!(
                display.contains(expected_phrase),
                "WriteIndicator({ind:?}).display() = {display:?} should contain {expected_phrase:?}"
            );
            // Humanized output must still cite the raw token so power users
            // can correlate with their command text.
            assert!(
                display.contains(ind.trim()),
                "humanized display should still cite raw token `{ind}`: {display:?}"
            );
        }
    }

    /// UX: `UnknownPrefix::display()` must frame the issue as a *risk* (the
    /// command may modify the system) rather than as an implementation
    /// detail ("not in allowlist"). Non-developer users don't know what an
    /// allowlist is, but they understand "may modify your system".
    #[test]
    fn approval_reason_unknown_prefix_display_is_risk_framed() {
        let display = BashApprovalReason::UnknownPrefix("foobar".to_string()).display();
        assert!(
            display.contains("foobar"),
            "display must cite the unknown token: {display}"
        );
        assert!(
            display.to_lowercase().contains("modify")
                || display.to_lowercase().contains("unrecognized")
                || display.to_lowercase().contains("unknown"),
            "display should frame as risk/unknown, not allowlist jargon: {display}"
        );
        // Negative assertion: the old technical-jargon phrase must not
        // reappear (guards against accidental revert).
        assert!(
            !display.contains("allowlist"),
            "display must not leak `allowlist` jargon: {display}"
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
}
