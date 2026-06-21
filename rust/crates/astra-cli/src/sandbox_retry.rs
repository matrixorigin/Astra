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

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Derive which directory should be added to the sandbox's allow-list
/// when a tool returned SANDBOX_DENIED for the given arguments.
///
/// Priority:
/// 1. Explicit file path-like argument fields (`path`, `file_path`, `file`,
///    `notebook_path`) - take parent, the path itself when it is an existing
///    directory, or the file itself when the parent would be `/`.
/// 2. Explicit working-directory fields (`cwd`, `workdir`, `working_dir`) -
///    take the directory itself.
/// 3. `args.command` (bash shape) - extract the first concrete path token
///    (`/abs/path`, `~/path`, `$HOME/path`, or `${HOME}/path`) and apply the
///    same parent logic.
///
/// Returns `None` when no safe concrete path can be derived (e.g. relative
/// paths, traversal, ambiguous shell syntax, or protected credential paths).
///
/// **Never returns `/` or a protected sensitive path** - widening either would
/// defeat the sandbox contract.
#[must_use]
pub fn sandbox_expand_dir_from_args(args: &Value) -> Option<PathBuf> {
    if let Some(dir) = sandbox_expand_dir_from_path_args(args) {
        return Some(dir);
    }

    args.get("command")
        .and_then(Value::as_str)
        .and_then(extract_first_sandbox_expand_path)
        .and_then(|p| sandbox_expand_dir_from_pathish(&p))
}

/// Derive a sandbox expansion directory from tool arguments and, when the
/// arguments are not enough, from the sandbox-denied message itself.
///
/// This covers bash commands such as `cat ~/repo/file`: the command argument
/// may not carry a literal absolute path, but the denial is recoverable because
/// home-directory references resolve deterministically for the local executor.
#[must_use]
pub fn sandbox_expand_dir_from_denial(args: &Value, sandbox_msg: &str) -> Option<PathBuf> {
    if let Some(dir) = sandbox_expand_dir_from_path_args(args) {
        return Some(dir);
    }

    if let Some(dir) = sandbox_expand_dir_from_denial_message(sandbox_msg) {
        return Some(dir);
    }

    args.get("command")
        .and_then(Value::as_str)
        .and_then(extract_first_sandbox_expand_path)
        .and_then(|p| sandbox_expand_dir_from_pathish(&p))
}

fn sandbox_expand_dir_from_path_args(args: &Value) -> Option<PathBuf> {
    for key in SANDBOX_PATH_ARG_KEYS {
        if let Some(dir) = args
            .get(*key)
            .and_then(Value::as_str)
            .and_then(sandbox_expand_dir_from_pathish)
        {
            return Some(dir);
        }
    }
    for key in SANDBOX_DIR_ARG_KEYS {
        if let Some(dir) = args
            .get(*key)
            .and_then(Value::as_str)
            .and_then(sandbox_expand_dir_from_dir_pathish)
        {
            return Some(dir);
        }
    }
    None
}

const SANDBOX_PATH_ARG_KEYS: &[&str] = &["path", "file_path", "file", "notebook_path"];
const SANDBOX_DIR_ARG_KEYS: &[&str] = &["cwd", "workdir", "working_dir"];

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
        let token = trim_shell_path_token(&raw);
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
            return Some(token.to_string());
        }
        // Windows absolute path: `C:\...`. Avoids indexing past the end
        // for short tokens (pre-fix bug: `&token[1..3]` panicked on any
        // 2-char or shorter token).
        let bytes = token.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return Some(token.to_string());
        }
    }
    None
}

fn extract_first_sandbox_expand_path(command: &str) -> Option<String> {
    let tokens = quote_aware_tokens(command);
    for raw in tokens {
        let token = trim_shell_path_token(&raw);
        if token.is_empty() {
            continue;
        }
        if token.starts_with('/') {
            if token.starts_with("//") || token.contains('$') {
                continue;
            }
            return Some(token.to_string());
        }
        if is_home_reference(token) {
            return Some(token.to_string());
        }
        let bytes = token.as_bytes();
        if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return Some(token.to_string());
        }
    }
    None
}

fn sandbox_expand_dir_from_denial_message(message: &str) -> Option<PathBuf> {
    let mut segments = message.split('\'');
    let mut before = segments.next().unwrap_or_default();
    while let Some(segment) = segments.next() {
        let after = segments.next().unwrap_or_default();
        if !quoted_segment_names_sandbox_boundary(before) {
            if quoted_segment_is_sensitive_path(segment) {
                return None;
            }
            if let Some(dir) = sandbox_expand_dir_from_pathish(segment) {
                return Some(dir);
            }
        }
        before = after;
    }
    None
}

fn quoted_segment_names_sandbox_boundary(before_quote: &str) -> bool {
    let before_quote = before_quote.trim_end().to_ascii_lowercase();
    before_quote.ends_with("project directory")
        || before_quote.ends_with("project root")
        || before_quote.ends_with("workspace directory")
        || before_quote.ends_with("workspace root")
}

fn quoted_segment_is_sensitive_path(segment: &str) -> bool {
    let token = trim_shell_path_token(segment);
    let Some(path) = expand_concrete_pathish(token) else {
        return false;
    };
    path.is_absolute() && sandbox_expand_path_is_sensitive(&path)
}

fn sandbox_expand_dir_from_pathish(pathish: &str) -> Option<PathBuf> {
    let token = trim_shell_path_token(pathish);
    if pathish_has_forbidden_segment(token) {
        return None;
    }
    let path = expand_concrete_pathish(token)?;
    checked_expand_path(path, false)
}

fn sandbox_expand_dir_from_dir_pathish(pathish: &str) -> Option<PathBuf> {
    let token = trim_shell_path_token(pathish);
    if pathish_has_forbidden_segment(token) {
        return None;
    }
    let path = expand_concrete_pathish(token)?;
    checked_expand_path(path, true)
}

fn checked_expand_path(path: PathBuf, directory_arg: bool) -> Option<PathBuf> {
    if !path.is_absolute() || path == Path::new("/") {
        return None;
    }
    if has_forbidden_component(&path) || sandbox_expand_path_is_sensitive(&path) {
        return None;
    }
    if directory_arg || path.is_dir() {
        return Some(path);
    }

    let parent = path.parent()?;
    if parent == Path::new("/") || parent.as_os_str().is_empty() {
        Some(path)
    } else {
        Some(parent.to_path_buf())
    }
}

fn expand_concrete_pathish(token: &str) -> Option<PathBuf> {
    if token.is_empty() || token.contains('\0') {
        return None;
    }
    if let Some(home_path) = expand_home_reference(token) {
        return Some(home_path);
    }
    if token.contains('$') {
        return None;
    }
    let path = PathBuf::from(token);
    path.is_absolute().then_some(path)
}

fn expand_home_reference(token: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if matches!(token, "~" | "$HOME" | "${HOME}") {
        return Some(home);
    }
    let suffix = token
        .strip_prefix("~/")
        .or_else(|| token.strip_prefix("$HOME/"))
        .or_else(|| token.strip_prefix("${HOME}/"))?;
    (!suffix.contains('$')).then(|| home.join(suffix))
}

fn is_home_reference(token: &str) -> bool {
    matches!(token, "~" | "$HOME" | "${HOME}")
        || token.starts_with("~/")
        || token.starts_with("$HOME/")
        || token.starts_with("${HOME}/")
}

fn trim_shell_path_token(token: &str) -> &str {
    token
        .trim()
        .trim_matches(['"', '\''])
        .trim_end_matches([';', '&', ')', ','])
        .trim_matches(['"', '\''])
}

fn has_forbidden_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
}

fn pathish_has_forbidden_segment(pathish: &str) -> bool {
    pathish
        .split(['/', '\\'])
        .any(|segment| matches!(segment, "." | ".."))
}

fn sandbox_expand_path_is_sensitive(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "/etc/shadow" | "/etc/sudoers" | "/etc/passwd"
    ) {
        return true;
    }
    if lower.starts_with("/etc/sudoers.d/") {
        return true;
    }
    for marker in [
        "/.ssh",
        "/.aws",
        "/.kube",
        "/.gnupg",
        "/.azure",
        "/.config/gh",
        "/.config/gcloud",
        "/.docker/config.json",
        "/.git-credentials",
        "/.netrc",
    ] {
        if lower == marker.trim_start_matches('/')
            || lower.ends_with(marker)
            || lower.contains(&format!("{marker}/"))
        {
            return true;
        }
    }
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        file_name,
        "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_ed25519_sk" | ".env"
    )
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
    sandbox_denied_message(output).is_some()
}

/// Strip the SANDBOX_DENIED_PREFIX and return just the message body.
///
/// Returns `None` if the string doesn't carry the prefix.
#[must_use]
pub fn sandbox_denied_message(output: &str) -> Option<Cow<'_, str>> {
    if let Some(message) = output.strip_prefix(SANDBOX_DENIED_PREFIX) {
        return Some(Cow::Borrowed(message));
    }

    if let Ok(value) = serde_json::from_str::<Value>(output)
        && let Some(message) = value
            .get("error")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .and_then(|message| message.strip_prefix(SANDBOX_DENIED_PREFIX))
    {
        return Some(Cow::Owned(message.to_string()));
    }

    let trimmed = output.strip_prefix("Error: ").unwrap_or(output);
    if trimmed.contains("outside the project directory")
        && (trimmed.contains("sandbox approval") || trimmed.contains("Ask the user"))
    {
        return Some(Cow::Borrowed(trimmed.trim_end_matches('.')));
    }

    None
}

#[must_use]
pub fn sandbox_retry_no_expand_dir_output(tool: &str, sandbox_msg: &str) -> String {
    format!(
        "Error: {tool} was blocked by the sandbox, but Astra could not safely choose a concrete non-sensitive directory to approve.\nPath check: {sandbox_msg}"
    )
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
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
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
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::{
        SANDBOX_DENIED_PREFIX, extract_first_absolute_path, is_sandbox_denied,
        sandbox_denied_message, sandbox_expand_dir_from_args, sandbox_expand_dir_from_denial,
        sandbox_retry_no_expand_dir_output,
    };
    use serde_json::json;
    use std::path::PathBuf;

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
    fn expand_dir_from_home_reference_path_arg() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let args = json!({"path": "~/astra-review-guide.md"});
        assert_eq!(sandbox_expand_dir_from_args(&args), Some(home));
    }

    #[test]
    fn expand_dir_from_bash_home_env_reference() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let args = json!({"command": "cat ${HOME}/external-workspace/Tool.ts"});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(home.join("external-workspace"))
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
    fn expand_dir_handles_shell_escaped_spaces_in_command_paths() {
        let args = json!({"command": r#"cat /home/user/My\ Project/report.md"#});
        assert_eq!(
            sandbox_expand_dir_from_args(&args),
            Some(PathBuf::from("/home/user/My Project"))
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

    #[test]
    fn expand_dir_rejects_sensitive_path_args() {
        for args in [
            json!({"path": "/home/user/.ssh/id_rsa"}),
            json!({"path": "/etc/shadow"}),
            json!({"cwd": "/home/user/.ssh"}),
        ] {
            assert_eq!(
                sandbox_expand_dir_from_args(&args),
                None,
                "sandbox expansion must not derive a directory for sensitive path args: {args}"
            );
        }
    }

    #[test]
    fn expand_dir_from_denial_message_when_args_are_not_enough() {
        let args = json!({"command": "custom_reader --target outside"});
        let message = "The command references '/home/user/external-workspace/Tool.ts' which is outside the project directory '/home/user/project'.";
        assert_eq!(
            sandbox_expand_dir_from_denial(&args, message),
            Some(PathBuf::from("/home/user/external-workspace"))
        );
    }

    #[test]
    fn expand_dir_from_denial_message_home_reference() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let args = json!({"command": "cat ~/external-workspace/Tool.ts"});
        let message = "The command references '~/external-workspace/Tool.ts' which is outside the project directory '/project'.";
        assert_eq!(
            sandbox_expand_dir_from_denial(&args, message),
            Some(home.join("external-workspace"))
        );
    }

    #[test]
    fn expand_dir_from_denial_message_does_not_use_project_root_fallback() {
        let args = json!({"path": "../secret.txt"});
        let message = "Path '../secret.txt' is outside the project directory '/home/user/project'; sandbox approval is required for this external path.";
        assert_eq!(sandbox_expand_dir_from_denial(&args, message), None);
    }

    #[test]
    fn expand_dir_from_denial_message_rejects_sensitive_target() {
        let args = json!({"command": "external_reader --target secret"});
        let message =
            "Path '/home/user/.ssh/id_rsa' is outside the project directory '/home/user/project'.";
        assert_eq!(
            sandbox_expand_dir_from_denial(&args, message),
            None,
            "sandbox retry must fail closed if a denial names a sensitive path"
        );
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

    #[test]
    fn extract_path_handles_shell_escaped_spaces() {
        assert_eq!(
            extract_first_absolute_path(r#"cat /path\ with\ spaces/file"#),
            Some("/path with spaces/file".to_string())
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
    fn is_sandbox_denied_detects_structured_write_file_error() {
        let output = json!({
            "success": false,
            "error": "SANDBOX_DENIED: Path '/home/user/out.md' is outside the project directory '/home/user/project'; sandbox approval is required for this external path."
        })
        .to_string();

        assert!(is_sandbox_denied(&output));
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
        assert_eq!(msg.as_ref(), "path outside");
    }

    #[test]
    fn sandbox_denied_message_strips_structured_error_prefix() {
        let output = json!({
            "success": false,
            "error": "SANDBOX_DENIED: Path '/home/user/out.md' is outside the project directory '/home/user/project'; sandbox approval is required for this external path."
        })
        .to_string();

        let msg = sandbox_denied_message(&output).unwrap();
        assert_eq!(
            msg.as_ref(),
            "Path '/home/user/out.md' is outside the project directory '/home/user/project'; sandbox approval is required for this external path."
        );
    }

    #[test]
    fn sandbox_denied_message_none_for_non_prefixed() {
        assert!(sandbox_denied_message("ok: contents").is_none());
    }

    #[test]
    fn sandbox_retry_no_expand_dir_output_is_not_retryable_denial() {
        let output = sandbox_retry_no_expand_dir_output(
            "read_file",
            "Path '../secret.txt' is outside the project directory '/home/user/project'.",
        );

        assert!(!output.contains(SANDBOX_DENIED_PREFIX));
        assert!(!is_sandbox_denied(&output));
        assert!(output.contains("concrete non-sensitive directory"));
        assert!(output.contains("Path check:"));
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
