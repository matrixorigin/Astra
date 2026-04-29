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

/// Split a compound command (using `|`, `;`, `\n`, `&&`, `||`) into
/// individual segments suitable for per-segment intent analysis.
fn split_segments(command: &str) -> Vec<String> {
    let lower_friendly = command.trim();
    lower_friendly
        .split(['|', ';', '\n'])
        .flat_map(|chunk| chunk.split("&&"))
        .flat_map(|chunk| chunk.split("||"))
        .map(|s| s.trim().to_string())
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
    let has_redirect = segment_lower.contains(">>")
        || segment_lower
            .find('>')
            .is_some_and(|i| i > 0 && segment_lower.as_bytes().get(i - 1) != Some(&b'-'));
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
    ];
    MUTATING_PREFIXES.iter().any(|p| head.starts_with(p))
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
    let mut mutating = false;
    let mut read_targets: Vec<String> = Vec::new();
    for segment in split_segments(command) {
        let lower = segment.to_lowercase();
        if segment_is_mutating(&lower) {
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
    fn redirect_is_mutating() {
        assert!(intent("echo hi > foo.txt").mutating);
        assert!(intent("echo hi >>foo.txt").mutating);
        assert!(!intent("rg --files | head -n 50").mutating);
    }

    /// Regression: benign fd redirects must NOT be classified as mutating.
    /// Covers every pattern stripped by `strip_benign_fd_redirects`. Keep in
    /// sync with the twin guard in `astra_turn_core::cloud_approval_policy`.
    #[test]
    fn benign_fd_redirects_are_not_mutating() {
        assert!(!intent("cargo check 2>&1").mutating);
        assert!(!intent("cargo check 1>&2").mutating);
        assert!(!intent("cargo check 2>/dev/null").mutating);
        assert!(!intent("cargo check 1>/dev/null").mutating);
        assert!(!intent("cargo check >/dev/null").mutating);
        assert!(!intent("cargo check &>/dev/null").mutating);
        assert!(!intent("cargo check 2>&1 | head -50").mutating);
        // Append-form combined redirect: `&>>` MUST still be stripped so the
        // surviving `>>` in a literal filename doesn't false-positive.
        assert!(!intent("cargo check &>> /tmp/unused_log").mutating);
    }

    /// Residual-risk guard: the target filename surviving after redirect-
    /// stripping MUST NOT accidentally match any mutating-verb heuristic in
    /// `segment_is_mutating`. Twin of
    /// `astra_turn_core::cloud_approval_policy::benign_fd_redirect_target_filenames_are_inert`;
    /// keep the two corpora aligned.
    #[test]
    fn benign_fd_redirect_target_filenames_are_inert() {
        // Filenames textually containing mutating-verb substrings must not
        // flip the intent to mutating after redirect stripping.
        assert!(!intent("cargo check &>> /tmp/rm_me.log").mutating);
        assert!(!intent("cargo check &>> /var/log/mv_state").mutating);
        assert!(!intent("cargo check &>> ./cp_backup.log").mutating);
        assert!(!intent("cargo check &> /tmp/chmod.out").mutating);
        assert!(!intent("cargo check 2> /tmp/git_commit_trace.log").mutating);
        assert!(!intent("cargo check &>> /tmp/rm_me.log && echo done").mutating);
        // Malformed tail (no target after `2>`): conservative contract —
        // the dangling `>` is left in place so it trips the mutation scan.
        // Twin of `cloud_approval_policy::…::benign_fd_redirect_target_filenames_are_inert`;
        // if you change this, change both sides.
        assert!(intent("cargo check 2>").mutating);
        // Non-ASCII filename: UTF-8 sequence must survive the byte-level
        // scan intact (no mojibake that could hit a write-verb substring).
        assert!(!intent("cargo check &>> /tmp/日志.log").mutating);
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
        // when dangling. Previously `.replace("&>", " ")` silently ate the
        // operator and made `cargo check &>` look read-only.
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
        // Sanity: genuine fd redirects (digit at a token boundary) remain
        // non-mutating after stripping.
        assert!(!intent("cargo check 2>/tmp/log").mutating);
        assert!(!intent("2>/tmp/log cargo check").mutating);
        assert!(!intent("true | 2>/tmp/log cargo check").mutating);
    }

    #[test]
    fn sed_in_place_is_mutating_sed_n_is_not() {
        assert!(intent("sed -i 's/a/b/' foo.rs").mutating);
        assert!(!intent("sed -n '1,20p' foo.rs").mutating);
    }

    #[test]
    fn compound_and_sudo_prefixes_handled() {
        assert!(intent("cd /tmp && mv x y").mutating);
        assert!(intent("sudo rm -rf /tmp/foo").mutating);
        assert!(!intent("cd rust && cat src/lib.rs").mutating);
    }

    #[test]
    fn install_commands_are_mutating() {
        assert!(intent("npm install react").mutating);
        assert!(intent("pnpm install").mutating);
    }

    // ─── Read-target extraction (NEW, fixes c49bc4a3 deadlock) ───────────

    #[test]
    fn cat_extracts_single_file_target() {
        let r = intent("cat rust/crates/astra-sandbox/src/policy.rs");
        assert!(!r.mutating);
        assert_eq!(
            r.read_targets,
            vec!["rust/crates/astra-sandbox/src/policy.rs"]
        );
    }

    #[test]
    fn cat_extracts_multiple_file_targets() {
        let r = intent("cat a.rs b.rs c.rs");
        assert_eq!(r.read_targets, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn head_with_flag_value_skips_value_token() {
        let r = intent("head -n 50 src/main.rs");
        assert_eq!(r.read_targets, vec!["src/main.rs"]);
    }

    #[test]
    fn head_with_compact_flag_still_extracts_path() {
        let r = intent("head -n50 src/main.rs");
        assert_eq!(r.read_targets, vec!["src/main.rs"]);
    }

    #[test]
    fn tail_with_long_flag() {
        let r = intent("tail --lines=20 log.txt");
        assert_eq!(r.read_targets, vec!["log.txt"]);
    }

    #[test]
    fn sed_n_script_is_not_counted_as_path() {
        let r = intent("sed -n '80,140p' rust/crates/runtime/src/turn/agentic_loop_lifecycle.rs");
        assert!(!r.mutating);
        assert_eq!(
            r.read_targets,
            vec!["rust/crates/runtime/src/turn/agentic_loop_lifecycle.rs"]
        );
    }

    #[test]
    fn sed_without_n_flag_does_not_register_paths() {
        // Bare `sed 's/a/b/' file` is still technically a read (sed defaults
        // to print) but we stay conservative — the model should use `sed -n`
        // for read-only inspection explicitly.
        let r = intent("sed 's/a/b/' foo.rs");
        assert!(r.read_targets.is_empty());
    }

    #[test]
    fn wc_extracts_path() {
        assert_eq!(
            intent("wc -l src/main.rs").read_targets,
            vec!["src/main.rs"]
        );
    }

    #[test]
    fn compound_reads_harvested_per_segment() {
        let r = intent("cat a.rs && cat b.rs");
        assert!(!r.mutating);
        assert_eq!(r.read_targets, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn compound_mixed_does_not_harvest_from_mutating_segment() {
        // Mutating segment must NOT contribute its positional token as a
        // read target — we don't want to falsely mark a file "read" just
        // because it's being overwritten.
        let r = intent("cat a.rs && rm b.rs");
        assert!(r.mutating);
        assert_eq!(r.read_targets, vec!["a.rs"]);
    }

    #[test]
    fn globs_and_special_tokens_are_not_registered() {
        let r = intent("cat *.rs");
        assert!(r.read_targets.is_empty());
        let r = intent("cat -");
        assert!(r.read_targets.is_empty());
    }

    #[test]
    fn ambiguous_verbs_are_ignored() {
        // `grep`, `find`, `awk` intentionally not extracted — too
        // ambiguous (patterns, recursive modes, multiple positional args).
        assert!(intent("grep pat src/main.rs").read_targets.is_empty());
        assert!(intent("find . -name '*.rs'").read_targets.is_empty());
        assert!(intent("awk '{print}' src/main.rs").read_targets.is_empty());
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        let r = intent("cat \"path with spaces.rs\"");
        assert_eq!(r.read_targets, vec!["path with spaces.rs"]);
        let r = intent("cat 'another path.rs'");
        assert_eq!(r.read_targets, vec!["another path.rs"]);
    }

    #[test]
    fn pipe_to_head_harvests_cat_target() {
        let r = intent("cat big.log | head -n 50");
        assert!(!r.mutating);
        assert!(r.read_targets.contains(&"big.log".to_string()));
    }

    #[test]
    fn duplicates_deduped() {
        let r = intent("cat a.rs && cat a.rs");
        assert_eq!(r.read_targets, vec!["a.rs"]);
    }

    #[test]
    fn empty_and_garbage_inputs_safe() {
        assert_eq!(analyze_bash_command(""), BashIntent::default());
        assert_eq!(analyze_bash_command("   \n\n   "), BashIntent::default());
    }
}
