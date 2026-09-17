//! Shared bash-command intent classifier.
//!
//! Two independent file-read taxonomies used to exist in this codebase: the
//! agentic-loop mutation classifier (which correctly treats `cat` as a
//! read-only call) and the edge-tools `file_state` read-tracking hash map
//! (which only gets populated by the dedicated `read_file` tool). The
//! mismatch created a deadlock failure mode — a model that uses
//! `bash cat /path` for inspection would then be blocked from editing the
//! same file because the "read-before-write" gate never saw the read.
//!
//! This module centralises the classifier so both systems see the same
//! picture: every bash command is analysed once, yielding both its mutation
//! verdict and the list of file paths it reads. The edge-tools bash
//! dispatcher can then register the extracted targets with `file_state`.
//!
//! The parser is intentionally conservative: it only reports a read target
//! when the path is unambiguous (positional argument to a known pure-read
//! verb such as `cat`/`head`/`tail`/`sed -n`/`wc`/`nl`/`od`/`xxd`/`file`/
//! `stat`/`less`/`more`). Ambiguous verbs like `grep`, `awk`, `find` are
//! skipped on purpose: a false-positive registration (registering something
//! that isn't a real file) is harmless, but a false-positive *classification*
//! of a mutating command as read-only would be dangerous.

/// Result of analysing a single bash command.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BashIntent {
    /// `true` if any pipeline segment mutates the workspace
    /// (redirect, `sed -i`, `rm`, `mv`, `cp`, …).
    pub mutating: bool,
    /// Paths that were clearly targeted by a pure read verb in a
    /// non-mutating segment. Order preserved; de-duplicated.
    pub read_targets: Vec<String>,
}

/// Split a compound command at validated top-level control and pipeline
/// operators.  Do not use string splitting here: review commands routinely
/// contain `||`/`&&` inside `$(...)`, and treating those as outer operators
/// turns a benign nested fd redirect such as `2>/dev/null` into a false write
/// receipt.  The shared evaluator tracks quotes, substitutions, and malformed
/// syntax consistently with the completion/observation consumers.
fn split_segments(command: &str) -> Vec<String> {
    let Some(control_segments) = astra_turn_core::evaluation::split_shell_control_segments(command)
    else {
        return vec![command.trim().to_string()];
    };
    control_segments
        .into_iter()
        .flat_map(|control| {
            astra_turn_core::evaluation::split_shell_pipeline_segments(control)
                .unwrap_or_else(|| vec![control])
        })
        .map(str::trim)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Strip leading wrappers like `sudo`, `nohup`, `time`, and KEY=VALUE env
/// assignments so the head token is the real program name.
fn strip_leading_wrappers(segment: &str) -> &str {
    let mut head = segment.trim();
    loop {
        let trimmed = head
            .trim_start_matches("sudo ")
            .trim_start_matches("nohup ")
            .trim_start_matches("time ")
            .trim_start();
        if trimmed == head {
            // Also skip leading `KEY=VALUE` env assignments.
            if let Some((first, rest)) = head.split_once(char::is_whitespace) {
                if first.contains('=') && !first.starts_with('-') {
                    head = rest.trim_start();
                    continue;
                }
            }
            break;
        }
        head = trimmed;
    }
    head
}

/// Returns true if any character in this segment triggers a workspace
/// mutation (redirect, in-place edit, known mutating verb).
///
/// Benign fd-redirect stripping (`2>&1`, `1>&2`, `&>/dev/null`, `>/dev/null`,
/// …) is delegated to
/// [`astra_turn_core::cloud_approval_policy::strip_benign_fd_redirects`] —
/// the single source of truth shared with the permission gate so the two
/// classifiers cannot drift. See the drift-guard test
/// `tool_side_effects::read_only_permission_implies_non_mutating_cache_classification`.
fn segment_is_mutating(segment_lower: &str) -> bool {
    let normalized =
        astra_turn_core::cloud_approval_policy::strip_benign_fd_redirects(segment_lower);
    let segment_lower = normalized.as_str();
    // `cmd > file` and `cmd >file` (no space). `>>` first to avoid double-count.
    let has_redirect = segment_lower.contains(">>") || contains_unquoted_redirect(segment_lower);
    if segment_lower.contains("apply_patch")
        || has_redirect
        || segment_lower.contains("sed -i")
        || segment_lower.contains("perl -pi")
        || segment_lower.contains("tee ")
    {
        return true;
    }
    let head = strip_leading_wrappers(segment_lower);
    const MUTATING_PREFIXES: &[&str] = &[
        "mv ",
        "cp ",
        "rm ",
        "mkdir ",
        "touch ",
        "chmod ",
        "chown ",
        "ln ",
        "npm install",
        "pnpm install",
        "yarn install",
        "cargo fix",
        "go mod tidy",
        // Git commands that rewrite the working tree.  Read-only git
        // inspection (status/log/diff/show) is intentionally absent.
        "git add ",
        "git checkout ",
        "git clean",
        "git commit",
        "git merge",
        "git mv ",
        "git reset",
        "git restore ",
        "git rm ",
        "git stash ",
    ];
    MUTATING_PREFIXES.iter().any(|p| head.starts_with(p))
}

/// Detect a real shell output redirect without treating comparison operators
/// in quoted scripts (for example awk's `NR>=5380`) as writes.  The canonical
/// approval parser remains authoritative for execution; this scanner is only
/// the positive post-execution mutation-shape fallback and therefore keeps
/// malformed/unquoted redirects conservative.
fn contains_unquoted_redirect(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            continue;
        }
        if byte == b'>' && index > 0 && bytes.get(index - 1) != Some(&b'-') {
            return true;
        }
    }
    false
}

/// Simple tokenizer for a single command segment. We don't implement full
/// POSIX word-splitting — only enough to recognise positional file paths
/// passed to read verbs. Handles single/double-quoted spans.
fn tokenize(segment: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for ch in segment.chars() {
        if escape {
            cur.push(ch);
            escape = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escape = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Known pure-read verbs and how many value-flag arguments they consume.
/// For each, we list flags that take a following value (so we can skip the
/// value along with the flag). Any remaining non-flag token is treated as a
/// file path.
struct ReadVerbSpec {
    /// Flags that consume the next token as a value (e.g. `head -n 50`).
    value_flags: &'static [&'static str],
    /// Flags that are strict no-value switches (we still drop them).
    switch_prefixes: &'static [&'static str],
}

fn read_verb_spec(verb: &str) -> Option<ReadVerbSpec> {
    Some(match verb {
        "cat" | "nl" | "od" | "xxd" | "less" | "more" | "file" | "stat" => ReadVerbSpec {
            value_flags: &[],
            switch_prefixes: &["-"],
        },
        "head" | "tail" => ReadVerbSpec {
            value_flags: &["-n", "-c", "--lines", "--bytes"],
            switch_prefixes: &["-"],
        },
        "wc" => ReadVerbSpec {
            value_flags: &[],
            switch_prefixes: &["-"],
        },
        // `sed -n '<script>' FILE` is a pure read. `sed -i` is already
        // caught as mutating; we reach this branch only if the segment was
        // not mutating, but we still require the `-n` flag to be present so
        // that bare `sed` (which usually just prints) isn't mistaken for a
        // more aggressive pattern.
        "sed" => ReadVerbSpec {
            value_flags: &[],
            switch_prefixes: &["-"],
        },
        _ => return None,
    })
}

fn looks_like_path_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token == "-" || token == "/dev/null" || token == "/dev/stdin" {
        return false;
    }
    // Reject obvious globs — we can't resolve them without invoking the
    // shell, and registering a glob pattern as a read path would be wrong.
    if token.contains('*') || token.contains('?') || token.contains('[') {
        return false;
    }
    // Reject tokens that are actually shell operators that slipped through
    // tokenization (shouldn't happen, but be safe).
    if token.starts_with('<') || token.starts_with('>') {
        return false;
    }
    true
}

fn extract_read_targets_from_segment(segment: &str) -> Vec<String> {
    let tokens = tokenize(segment);
    let head = strip_leading_wrappers(segment);
    let head_verb = head.split_whitespace().next().unwrap_or("");
    let Some(spec) = read_verb_spec(head_verb) else {
        return Vec::new();
    };
    // For `sed` require `-n` to confirm pure-read usage.
    if head_verb == "sed" && !tokens.iter().any(|t| t == "-n" || t.starts_with("-n")) {
        return Vec::new();
    }
    // Skip the wrapper tokens (sudo/nohup/time/KEY=VAL) and the verb itself.
    let wrapper_prefix_len = tokens.len().saturating_sub(head.split_whitespace().count());
    let mut iter = tokens.iter().skip(wrapper_prefix_len + 1).peekable();
    let mut out: Vec<String> = Vec::new();
    // sed pattern-script is the first non-flag positional; we need to skip it.
    // Track whether we've seen the script (for sed).
    let mut sed_script_consumed = head_verb != "sed";

    while let Some(tok) = iter.next() {
        // Flag handling.
        if tok.starts_with('-') && tok.len() > 1 {
            if spec.value_flags.iter().any(|f| *f == tok) {
                iter.next(); // consume the value
                continue;
            }
            // `-n50` compact form: no separate value needed.
            if spec.switch_prefixes.iter().any(|p| tok.starts_with(p)) {
                continue;
            }
        }
        // For sed, the first positional is the script — skip it.
        if !sed_script_consumed {
            sed_script_consumed = true;
            continue;
        }
        if looks_like_path_token(tok) {
            out.push(tok.clone());
        }
    }
    out
}

/// Analyse a bash command end-to-end. Returns mutation verdict and any
/// clearly-addressed read targets.
pub fn analyze_bash_command(command: &str) -> BashIntent {
    // A command which the canonical permission classifier proved read-only
    // must not be reclassified as a mutation by this module's deliberately
    // broader fallback substring heuristics. Besides avoiding cache churn,
    // this preserves the security invariant that authorization and mutation
    // receipts describe one semantic command rather than two parsers' views
    // of its source spelling (for example `echo 'apply_patch'`).
    let canonically_read_only =
        astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command);
    let mut mutating = false;
    let mut read_targets: Vec<String> = Vec::new();
    for segment in split_segments(command) {
        let lower = segment.to_lowercase();
        if !canonically_read_only && segment_is_mutating(&lower) {
            mutating = true;
            continue; // Don't harvest reads from a mutating segment.
        }
        for path in extract_read_targets_from_segment(&segment) {
            if !read_targets.iter().any(|p| p == &path) {
                read_targets.push(path);
            }
        }
    }
    BashIntent {
        mutating,
        read_targets,
    }
}

/// Backwards-compatible shortcut used by the agentic loop classifier.
pub fn bash_command_looks_mutating(command: &str) -> bool {
    analyze_bash_command(command).mutating
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(cmd: &str) -> BashIntent {
        analyze_bash_command(cmd)
    }

    // ─── Mutation classification (regressions from prior code) ───────────

    #[test]
    fn mutation_classification() {
        // Redirects
        assert!(intent("echo hi > foo.txt").mutating);
        assert!(intent("echo hi >>foo.txt").mutating);
        assert!(!intent("rg --files | head -n 50").mutating);
        // Sed
        assert!(intent("sed -i 's/a/b/' foo.rs").mutating);
        assert!(!intent("sed -n '1,20p' foo.rs").mutating);
        // Compound / sudo
        assert!(intent("cd /tmp && mv x y").mutating);
        assert!(intent("sudo rm -rf /tmp/foo").mutating);
        assert!(!intent("cd workspace && cat src/lib.rs").mutating);
        // Package managers
        assert!(intent("npm install react").mutating);
        assert!(intent("pnpm install").mutating);
    }

    #[test]
    fn nested_shell_control_operators_do_not_forge_a_mutation() {
        let review = r#"cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)" && git show HEAD:src/lib.rs | sed -n '1,20p'"#;
        assert!(!intent(review).mutating);

        // A real mutating pipeline stage remains visible after top-level
        // splitting; only operators nested inside substitutions are ignored.
        assert!(intent("git show HEAD:src/lib.rs | sed -i 's/a/b/' src/lib.rs").mutating);
    }

    #[test]
    fn quoted_literals_do_not_forge_workspace_mutations() {
        for command in [
            "echo '2>/dev/null'",
            "echo '>'",
            "echo 'apply_patch'",
            "echo 'rm file; git commit is prose'",
        ] {
            assert!(
                astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command),
                "permission classifier rejected {command}"
            );
            assert!(!intent(command).mutating, "mutation drift for {command}");
        }
    }

    /// Only fd forwarding and explicit /dev/null disposal are non-mutating.
    /// Redirecting any fd into an ordinary file creates or truncates it.
    #[test]
    fn benign_fd_redirects_are_not_mutating() {
        assert!(!intent("cargo check 2>&1").mutating);
        assert!(!intent("cargo check 1>&2").mutating);
        assert!(!intent("cargo check 2>/dev/null").mutating);
        assert!(!intent("cargo check 1>/dev/null").mutating);
        assert!(!intent("cargo check >/dev/null").mutating);
        assert!(!intent("cargo check &>/dev/null").mutating);
        assert!(!intent("cargo check 2>&1 | head -50").mutating);
        assert!(intent("cargo check &>> /tmp/unused_log").mutating);
    }

    /// The filename is irrelevant to the effect: every ordinary redirect
    /// target is a write and must invalidate read evidence.
    #[test]
    fn ordinary_fd_redirect_targets_are_mutating() {
        assert!(intent("cargo check &>> /tmp/rm_me.log").mutating);
        assert!(intent("cargo check &>> /var/log/mv_state").mutating);
        assert!(intent("cargo check &>> ./cp_backup.log").mutating);
        assert!(intent("cargo check &> /tmp/chmod.out").mutating);
        assert!(intent("cargo check 2> /tmp/git_commit_trace.log").mutating);
        assert!(intent("cargo check &>> /tmp/rm_me.log && echo done").mutating);
        // Malformed tail (no target after `2>`): conservative contract —
        // the dangling `>` is left in place so it trips the mutation scan.
        assert!(intent("cargo check 2>").mutating);
        assert!(intent("cargo check &>> /tmp/日志.log").mutating);
    }

    /// Residual-risk guard: malformed trailing redirect (`cmd 2>` with no
    /// target after the operator) MUST fall back to conservative mutation
    /// classification on both gates. Shell itself errors on dangling
    /// redirects; we prefer false-positive approval over silent miss.
    /// Twin of
    /// `astra_turn_core::cloud_approval_policy::malformed_trailing_redirect_stays_conservative`;
    /// if you change this, change both sides.
    #[test]
    fn malformed_trailing_redirect_stays_conservative() {
        assert!(intent("cargo check 2>").mutating);
        assert!(intent("cargo check >").mutating);
        assert!(intent("cargo check 2>>").mutating);
        // Bash combined redirect variants must also fall back to mutating
        // when dangling.
        assert!(intent("cargo check &>").mutating);
        assert!(intent("cargo check &>>").mutating);
    }

    /// Residual-risk guard: fd-redirect detection MUST require a token
    /// boundary to the **left** of the digit. `echo a2>/tmp/x` — `a2` is the
    /// echo argument, `>` is a real stdout redirect writing to `/tmp/x`; the
    /// command genuinely mutates and must be classified as mutating. Twin of
    /// `astra_turn_core::cloud_approval_policy::fd_redirect_requires_left_token_boundary`;
    /// keep the two corpora aligned.
    #[test]
    fn fd_redirect_requires_left_token_boundary() {
        assert!(intent("echo a2>/tmp/x").mutating);
        assert!(intent("echo a2>>/tmp/x").mutating);
        assert!(intent("cargo check 2>/tmp/log").mutating);
        assert!(intent("2>/tmp/log cargo check").mutating);
        assert!(intent("true | 2>/tmp/log cargo check").mutating);
    }

    // ─── Read-target extraction (data-driven) ───────────────────────────

    #[test]
    fn read_target_extraction() {
        // Data-driven: single-segment read verbs
        let cases: &[(&str, &[&str], bool)] = &[
            (
                "cat crates/astra-sandbox/src/policy.rs",
                &["crates/astra-sandbox/src/policy.rs"],
                false,
            ),
            ("cat a.rs b.rs c.rs", &["a.rs", "b.rs", "c.rs"], false),
            ("head -n 50 src/main.rs", &["src/main.rs"], false),
            ("head -n50 src/main.rs", &["src/main.rs"], false),
            ("tail --lines=20 log.txt", &["log.txt"], false),
            ("wc -l src/main.rs", &["src/main.rs"], false),
            ("sed -n '80,140p' lifecycle.rs", &["lifecycle.rs"], false),
        ];
        for (cmd, expected, is_mutating) in cases {
            let r = intent(cmd);
            assert_eq!(r.mutating, *is_mutating, "mutating mismatch for: {cmd}");
            assert_eq!(r.read_targets, *expected, "targets mismatch for: {cmd}");
        }

        // Compound: &&, pipe, mixed mutating
        let r = intent("cat a.rs && cat b.rs");
        assert!(!r.mutating);
        assert_eq!(r.read_targets, vec!["a.rs", "b.rs"]);

        let r = intent("cat a.rs && rm b.rs");
        assert!(r.mutating);
        assert_eq!(r.read_targets, vec!["a.rs"]);

        let r = intent("cat big.log | head -n 50");
        assert!(!r.mutating);
        assert!(r.read_targets.contains(&"big.log".to_string()));
    }

    #[test]
    fn test_boundary_read_targets() {
        // Globs and special tokens not registered.
        assert!(intent("cat *.rs").read_targets.is_empty());
        assert!(intent("cat -").read_targets.is_empty());
        // Ambiguous verbs (grep, find, awk) intentionally not extracted.
        assert!(intent("grep pat src/main.rs").read_targets.is_empty());
        assert!(intent("find . -name '*.rs'").read_targets.is_empty());
        assert!(intent("awk '{print}' src/main.rs").read_targets.is_empty());
        // Quoted paths are unquoted.
        assert_eq!(
            intent("cat \"path with spaces.rs\"").read_targets,
            vec!["path with spaces.rs"]
        );
        assert_eq!(
            intent("cat 'another path.rs'").read_targets,
            vec!["another path.rs"]
        );
        // Duplicates deduped.
        assert_eq!(intent("cat a.rs && cat a.rs").read_targets, vec!["a.rs"]);
        // Bare sed (no -n) — do not register paths.
        assert!(intent("sed 's/a/b/' foo.rs").read_targets.is_empty());
    }

    #[test]
    fn empty_and_garbage_inputs_safe() {
        assert_eq!(analyze_bash_command(""), BashIntent::default());
        assert_eq!(analyze_bash_command("   \n\n   "), BashIntent::default());
    }
}
