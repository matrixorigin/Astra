//! Sandbox-denied retry helpers, extracted so both the sequential and
//! parallel tool-execution paths share one implementation.
//!
//! Background: when a tool returns [`SANDBOX_DENIED_PREFIX`], the runtime
//! should route through a re-prompt flow — ask the user for permission
//! (or auto-approve under [`PermissionMode::Auto`]), widen the sandbox,
//! then retry the tool. Historically only the sequential path in
//! `stream_render.rs` did this; the parallel batch path silently handed
//! the `SANDBOX_DENIED:` error string back to the model, which was
//! forced to ask the user manually — defeating auto mode entirely.
//! Observed in session `3b7ac18f`: 4 `~/claudecode/*` reads blocked,
//! 0 `sandbox_expand` approval events.
//!
//! This module factors out the pieces that are pure logic (no UI, no
//! stdin, no async state) so they can be unit-tested and called from
//! both paths.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Derive which directory should be added to the sandbox's allow-list
/// when a tool returned SANDBOX_DENIED for the given arguments.
///
/// Priority:
/// 1. `args.path` or `args.file_path` (file-tool shape) — take parent,
///    or the file itself when the parent would be `/`.
/// 2. `args.command` (bash shape) — extract first absolute path token
///    and apply the same parent logic.
///
/// Returns `None` when no concrete path can be derived (e.g. tool
/// arguments carry only relative paths — those are already inside the
/// project root per the sandbox contract).
///
/// **Never returns `/`** — widening to root would defeat the sandbox.
#[must_use]
pub fn sandbox_expand_dir_from_args(args: &Value) -> Option<PathBuf> {
    // Only absolute paths are candidates for sandbox expansion: relative
    // paths are already inside the project root per the sandbox
    // contract, so a SANDBOX_DENIED on a relative path indicates a
    // different issue (the relative path resolved to outside via `../`
    // traversal etc.) and the conservative move is to NOT auto-widen.
    //
    // We also reject paths containing `..` components: `canonicalize()`
    // isn't safe here because the target file may not yet exist (e.g. a
    // write tool creating a new file), so we can't resolve traversal
    // reliably. A token like `/allowed/../etc/passwd` would otherwise
    // widen to `/allowed` — but Path::parent is purely lexical and the
    // real parent after `..` resolution is `/etc`. Refusing to expand
    // keeps the sandbox tight; the user sees the original denial and
    // can resubmit with a clean absolute path.
    let parent_or_self = |p: &str| -> Option<PathBuf> {
        let path = Path::new(p);
        if !path.is_absolute() {
            return None;
        }
        // Reject `..` / `.` traversal tokens — see note above.
        // We check the raw string rather than Path::components because
        // components() silently drops `.` (so `/./etc` would normalize
        // to `/etc`) while we want to refuse the whole non-canonical
        // input rather than guess at the author's intent.
        for segment in p.split('/') {
            if segment == ".." || segment == "." {
                return None;
            }
        }
        let parent = path.parent()?;
        // Defensive: after normalization above, `parent == /` means the
        // file sits directly under root (e.g. `/passwd`). Expand exactly
        // the file, never the root directory.
        if parent == Path::new("/") || parent.as_os_str().is_empty() {
            Some(PathBuf::from(p))
        } else {
            Some(parent.to_path_buf())
        }
    };

    if let Some(p) = args
        .get("path")
        .or_else(|| args.get("file_path"))
        .and_then(Value::as_str)
        && let Some(dir) = parent_or_self(p)
    {
        return Some(dir);
    }

    args.get("command")
        .and_then(Value::as_str)
        .and_then(extract_first_absolute_path)
        .and_then(|p| parent_or_self(&p))
}

/// Extract the first absolute-path token from a bash command.
///
/// Scans whitespace-separated tokens for one starting with `/` (Unix
/// absolute path) or containing `:\` (Windows absolute path). Strips
/// surrounding quote characters. Returns `None` if no token matches.
///
/// This is the narrow version used by sandbox retry; it intentionally
/// does not attempt full shell parsing. Callers tolerate `None` by
/// skipping sandbox expansion — the user sees the original denial and
/// can re-submit with an explicit path argument.
#[must_use]
pub fn extract_first_absolute_path(command: &str) -> Option<String> {
    // Strip one level of paired quotes that surround the whole command
    // fragment we scan. We can't do full shell parsing here, but we do
    // need to recognize the common shape `cat "/etc/hosts"` so the
    // returned token is the path, not `/etc/hosts`.
    //
    // Strategy: split by unquoted whitespace first by doing a minimal
    // quote-aware tokenize, then look for a token that starts with `/`
    // or matches the Windows drive-letter pattern `X:\`.
    let tokens = quote_aware_tokens(command);
    for raw in tokens {
        // Strip trailing shell punctuation (`;`, `&`, `)`) — a path
        // token followed by `;` or `&` is still a concrete absolute
        // path to the sandbox; we must not hand back the punctuation.
        let token = raw.trim_end_matches([';', '&', ')']).to_string();
        if token.is_empty() {
            continue;
        }
        if token.starts_with('/') {
            // Reject UNC-like `//server/share` — those are not Unix
            // absolute paths and widening to `/` would be catastrophic.
            if token.starts_with("//") {
                continue;
            }
            // Reject unexpanded variable references like `$HOME/…` —
            // `$` never appears in a real absolute path; if the shell
            // didn't expand it, we can't validate the target.
            if token.contains('$') {
                continue;
            }
            return Some(token);
        }
        // Windows absolute path: `C:\...`. Avoids indexing past the end
        // for short tokens (pre-fix bug: `&token[1..3]` panicked on any
        // 2-char or shorter token).
        let bytes = token.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return Some(token);
        }
    }
    None
}

/// Prefix emitted by tool executors when a call is blocked by the
/// sandbox boundary check. Duplicated from `edge_tools::SANDBOX_DENIED_PREFIX`
/// so this module can be tested without a dependency on the whole
/// executor tree.
pub const SANDBOX_DENIED_PREFIX: &str = "SANDBOX_DENIED: ";

/// True when `output` is a sandbox-denied result from one of the edge
/// tools. Keep this the single check site so the prefix contract has
/// one consumer.
#[must_use]
pub fn is_sandbox_denied(output: &str) -> bool {
    output.starts_with(SANDBOX_DENIED_PREFIX)
}

/// Strip the SANDBOX_DENIED_PREFIX and return just the message body.
///
/// Returns `None` if the string doesn't carry the prefix.
#[must_use]
pub fn sandbox_denied_message(output: &str) -> Option<&str> {
    output.strip_prefix(SANDBOX_DENIED_PREFIX)
}

/// Minimal quote-aware tokenizer: splits on whitespace unless it's
/// inside paired single or double quotes. The quotes themselves are
/// dropped from the returned tokens. Unbalanced quotes fall through to
/// plain whitespace split.
fn quote_aware_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in input.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── sandbox_expand_dir_from_args ─────────────────────────────────────

    #[test]
    fn expand_dir_from_read_file_path() {
        let args = json!({"path": "/home/user/project/src/main.rs"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/home/user/project/src"))
        );
    }

    #[test]
    fn expand_dir_from_str_replace_file_path() {
        // Some tools use `file_path` instead of `path`.
        let args = json!({"file_path": "/home/user/project/Cargo.toml"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn expand_dir_from_bash_cat_command() {
        // This is the 3b7ac18f session's exact shape: `cat ~/foo/bar.ts`
        // is normalized to an absolute path by the shell wrapper before
        // it ever reaches sandbox validation; the denial message echoes
        // the absolute form.
        let args = json!({"command": "cat /home/user/outside/file.ts"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/home/user/outside"))
        );
    }

    #[test]
    fn expand_dir_from_bash_with_flags_and_pipes() {
        // Scanner must pick the path token regardless of position — not
        // just `parts[1]`. Real commands have flags before the path.
        let args = json!({"command": "head -n 50 /etc/hosts | grep localhost"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/etc"))
        );
    }

    #[test]
    fn expand_dir_root_level_file_stays_as_file() {
        // `/passwd` → parent is `/` — never widen to `/`; expand exactly
        // the file instead. Pinned because widening to `/` is a security
        // hazard the original inline logic was careful about.
        let args = json!({"path": "/passwd"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/passwd"))
        );
    }

    #[test]
    fn expand_dir_none_for_relative_only() {
        // Relative paths don't escape the project root; nothing to expand.
        let args = json!({"path": "src/main.rs"});
        assert_eq!(sandbox_expand_dir_from_args(&args), None);
    }

    #[test]
    fn expand_dir_none_for_bash_without_absolute_token() {
        let args = json!({"command": "git status --short"});
        assert_eq!(sandbox_expand_dir_from_args(&args), None);
    }

    #[test]
    fn expand_dir_prefers_path_over_command() {
        // `path` field is authoritative — `command` would be wrong to
        // consult when both are present (shouldn't happen, but defensive).
        let args = json!({
            "path": "/a/b.txt",
            "command": "cat /c/d.txt"
        });
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/a"))
        );
    }

    #[test]
    fn expand_dir_handles_quoted_paths_in_command() {
        let args = json!({"command": "cat \"/etc/hosts\""});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/etc"))
        );
    }

    #[test]
    fn expand_dir_rejects_parent_traversal_in_path() {
        // `/allowed/../etc/passwd` lexically has parent `/allowed/..`, which
        // Path::parent reports as `/allowed` — but after `..` resolution the
        // real parent is `/etc`. We can't canonicalize unresolved paths
        // safely (target may not exist yet), so the only correct answer is
        // to refuse to auto-widen. Pinned as a sandbox-escape guard.
        let args = json!({"path": "/allowed/../etc/passwd"});
        assert_eq!(sandbox_expand_dir_from_args(&args), None);
    }

    #[test]
    fn expand_dir_rejects_parent_traversal_in_command() {
        let args = json!({"command": "cat /allowed/../etc/passwd"});
        assert_eq!(sandbox_expand_dir_from_args(&args), None);
    }

    #[test]
    fn expand_dir_rejects_curdir_component() {
        // `/./etc/hosts` is harmless but non-canonical; rejecting is the
        // simpler contract than trying to partially-normalize.
        let args = json!({"path": "/./etc/hosts"});
        assert_eq!(sandbox_expand_dir_from_args(&args), None);
    }

    // ── extract_first_absolute_path ──────────────────────────────────────

    #[test]
    fn extract_path_first_absolute_wins() {
        assert_eq!(
            extract_first_absolute_path("grep -n foo /tmp/a /tmp/b"),
            Some("/tmp/a".to_string())
        );
    }

    #[test]
    fn extract_path_skips_relative_tokens() {
        assert_eq!(
            extract_first_absolute_path("cat src/main.rs /etc/hosts"),
            Some("/etc/hosts".to_string())
        );
    }

    #[test]
    fn extract_path_none_when_all_relative() {
        assert_eq!(
            extract_first_absolute_path("cd project && cargo build"),
            None
        );
    }

    #[test]
    fn extract_path_strips_surrounding_quotes() {
        assert_eq!(
            extract_first_absolute_path(r#"cat "/path with spaces/file""#),
            Some("/path with spaces/file".to_string())
        );
        // Trailing quote shouldn't leak into the returned string.
        assert!(
            !extract_first_absolute_path("echo '/a/b'")
                .unwrap()
                .contains('\'')
        );
    }

    // Ported from the legacy `stream_render::extract_first_absolute_path`
    // (now deleted) so the behaviour the sandbox retry depends on is
    // pinned in one place.

    #[test]
    fn extract_path_strips_trailing_semicolon() {
        assert_eq!(
            extract_first_absolute_path("cat /etc/passwd;"),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn extract_path_rejects_unexpanded_variable() {
        // `$HOME/.bashrc` shouldn't be widened to `$HOME/` — the shell
        // never expanded the var, so we can't locate the real parent.
        assert_eq!(extract_first_absolute_path("cat $HOME/.bashrc"), None);
    }

    #[test]
    fn extract_path_rejects_unc_path() {
        // `//server/share` is a UNC-style path; widening to `/` via
        // parent() would be a sandbox-escape hazard.
        assert_eq!(extract_first_absolute_path("cat //server/share"), None);
    }

    #[test]
    fn extract_path_empty_command() {
        assert_eq!(extract_first_absolute_path(""), None);
    }

    // ── SANDBOX_DENIED prefix helpers ───────────────────────────────────

    #[test]
    fn prefix_is_exactly_sandbox_denied() {
        // Pin the wire contract — any deviation breaks tool-output matching
        // in the re-prompt path. Must stay byte-exact with
        // `edge_tools::SANDBOX_DENIED_PREFIX`.
        assert_eq!(SANDBOX_DENIED_PREFIX, "SANDBOX_DENIED: ");
    }

    #[test]
    fn is_sandbox_denied_detects_prefix() {
        assert!(is_sandbox_denied(
            "SANDBOX_DENIED: The command references '/foo' which is outside …"
        ));
    }

    #[test]
    fn is_sandbox_denied_rejects_non_prefixed() {
        assert!(!is_sandbox_denied("ok: file contents…"));
        assert!(!is_sandbox_denied(
            "Error: something else. SANDBOX_DENIED:  … (not at start)"
        ));
    }

    #[test]
    fn sandbox_denied_message_strips_prefix() {
        let msg = sandbox_denied_message("SANDBOX_DENIED: path outside").unwrap();
        assert_eq!(msg, "path outside");
    }

    #[test]
    fn sandbox_denied_message_none_for_non_prefixed() {
        assert!(sandbox_denied_message("ok: contents").is_none());
    }

    // ── Integration invariant (regression guard for session 3b7ac18f)
    //
    // Context: in PermissionMode::Auto the user has explicitly opted into
    // "approve everything". A SANDBOX_DENIED must NOT bubble back to the
    // LLM unchanged — the expected flow is:
    //
    //   1. Detect prefix with `is_sandbox_denied`.
    //   2. Derive the expand dir with `sandbox_expand_dir_from_args`.
    //   3. Widen the executor's sandbox, then retry the tool.
    //
    // Session `3b7ac18f` turn 12-15 broke step 1 in the parallel batch
    // path (the check only ran in the sequential path). These tests
    // pin the helpers so whichever call site uses them gets the same
    // contract.

    #[test]
    fn auto_mode_contract_detects_denial_and_derives_dir() {
        // Simulate the exact tool output + args shape from session 3b7ac18f.
        let tool_output = "SANDBOX_DENIED: The command references \
                           '/home/user/claudecode/tools/FileReadTool/limits.ts' \
                           which is outside the project directory …";
        let tool_args = json!({
            "command": "cat /home/user/claudecode/tools/FileReadTool/limits.ts"
        });

        // Step 1: the prefix detector sees the denial.
        assert!(is_sandbox_denied(tool_output));

        // Step 2: the expand dir is derived from args.
        let dir = sandbox_expand_dir_from_args(&tool_args).expect("expand dir");
        assert_eq!(
            dir,
            PathBuf::from("/home/user/claudecode/tools/FileReadTool")
        );

        // Step 3: the message body survives stripping.
        let body = sandbox_denied_message(tool_output).expect("body");
        assert!(body.contains("outside the project directory"));
    }
}
