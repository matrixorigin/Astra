//! Normalized reads of OpenAI-style `function.arguments` (object or stringified JSON).
//!
//! **Canonical keys only** (no legacy aliases): file targets use `path`; shell tools use `command`.
//! Edge executors and tool schemas should emit these names; hints do not read `file_path`, `target_file`, or `cmd`.

use serde_json::Value;

use crate::cloud::approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind_with_args};

/// Parse `function.arguments` from an LLM tool call: either a JSON object or a string of JSON.
pub fn normalize_llm_function_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        v => v.clone(),
    }
}

/// Primary filesystem path from tool arguments (`path` only).
pub fn path_hint_from_args(args: &Value) -> Option<String> {
    args.get("path").and_then(Value::as_str).map(String::from)
}

/// Shell command line from tool arguments (`command` only).
pub fn command_hint_from_args(args: &Value) -> Option<&str> {
    args.get("command").and_then(Value::as_str)
}

/// Extract the persistent allow-rule prefix from a shell command.
///
/// Mirrors Claude Code's safety posture: only stable command+subcommand
/// shapes become reusable prefixes (`git commit`, `npm test`, `cargo test`).
/// Single-word commands, interpreter invocations (`python -c`, `bash -c`),
/// shell wrappers (`sudo`, `env`, `timeout`), flags, paths, and filenames fall
/// back to exact-command rules instead of broad `Bash(foo:*)` rules.
#[must_use]
pub fn normalized_argv_prefix(cmd: &str) -> String {
    const SHELL_METACHARS: &[&str] = &["|", ";", "&&", "||", ">", "<", ">>", "<<", "&"];

    let mut cmd = cmd.trim();
    if let Some((head, tail)) = cmd.split_once("&&")
        && head.trim_start().starts_with("cd ")
    {
        cmd = tail.trim();
    }

    let mut tokens = Vec::new();
    for raw in cmd.split_whitespace() {
        if SHELL_METACHARS.contains(&raw) {
            break;
        }
        if raw.starts_with('-') {
            break;
        }
        if tokens.is_empty() && looks_like_env_assignment(raw) {
            let Some((key, _)) = raw.split_once('=') else {
                return String::new();
            };
            if is_safe_env_prefix(key) {
                continue;
            }
            return String::new();
        }
        tokens.push(raw);
        if tokens.len() >= 2 {
            break;
        }
    }

    let [command, subcommand] = tokens.as_slice() else {
        return String::new();
    };
    if is_unsafe_bare_shell_prefix(command) || !looks_like_subcommand(subcommand) {
        return String::new();
    }

    format!("{command} {subcommand}")
}

fn looks_like_env_assignment(token: &str) -> bool {
    let Some((key, value)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !value.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

fn is_safe_env_prefix(key: &str) -> bool {
    matches!(
        key,
        "GOEXPERIMENT"
            | "GOOS"
            | "GOARCH"
            | "CGO_ENABLED"
            | "GO111MODULE"
            | "RUST_BACKTRACE"
            | "RUST_LOG"
            | "NODE_ENV"
            | "CI"
            | "PYTHONUNBUFFERED"
            | "PYTHONDONTWRITEBYTECODE"
            | "PYTEST_DISABLE_PLUGIN_AUTOLOAD"
            | "PYTEST_DEBUG"
            | "LANG"
            | "LANGUAGE"
            | "LC_ALL"
            | "LC_CTYPE"
            | "LC_TIME"
            | "CHARSET"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
            | "FORCE_COLOR"
            | "TZ"
    )
}

fn is_unsafe_bare_shell_prefix(command: &str) -> bool {
    is_unsafe_shell_prefix_token(command)
}

/// Single source of truth for shell/interpreter/wrapper command tokens that
/// must never anchor a broad allow rule (case-insensitive). Used by both the
/// argument-hint prefix extractor and the allow-rule safety classifier in
/// `PermissionRule::is_dangerous_bash_allow_shape`.
///
/// Invariants:
/// - All entries are ASCII lowercase; matching lowercases the input.
/// - Tokens here either spawn arbitrary code (shells, interpreters), elevate
///   privileges (`sudo`, `doas`, `pkexec`), or wrap an arbitrary inner
///   command (`env`, `xargs`, `nice`, `stdbuf`, `nohup`, `timeout`, `time`,
///   `exec`, `busybox`).
/// - Adding an entry tightens both code paths simultaneously; do not
///   duplicate this list elsewhere.
pub fn is_unsafe_shell_prefix_token(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        // Shells
        "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "csh"
            | "tcsh"
            | "ksh"
            | "dash"
            | "ash"
            | "cmd"
            | "powershell"
            | "pwsh"
            // Interpreters
            | "python"
            | "python2"
            | "python3"
            | "node"
            | "nodejs"
            | "deno"
            | "bun"
            | "ruby"
            | "perl"
            | "php"
            | "lua"
            // Wrappers that pass through to an arbitrary inner command
            | "env"
            | "xargs"
            | "nice"
            | "stdbuf"
            | "nohup"
            | "timeout"
            | "time"
            | "exec"
            | "busybox"
            // Privilege elevation
            | "sudo"
            | "doas"
            | "pkexec"
    )
}

fn looks_like_subcommand(token: &str) -> bool {
    // Subcommands are conventionally lowercase ASCII identifiers; allow `-`
    // and `_` in trailing chars (e.g. `make install_deps`, `cargo nextest-run`).
    // The first char must be a lowercase ASCII letter or digit so flag-like
    // tokens (`-v`, `--all`) and underscore-led tokens (`_foo`) don't sneak
    // through and get mistaken for subcommands. Rejects paths, redirection,
    // quoting, env-style `KEY=value`, etc.
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Raw hint used for **permission rule matching** — `starts_with`
/// checks against deny/allow rule patterns depend on this being the
/// naked `command` / `path` value, not a formatted preview.
///
/// Must NOT be changed to wrap or decorate the value: rule patterns
/// like `bash(rm -rf:*)` match against raw commands, so returning
/// "$ rm -rf ..." would silently stop blocking them. Previously this
/// function also drove the approval-dialog display label, which
/// coupled the two concerns; now the display label is generated
/// separately via [`crate::tool::policy::preview::render_preview`].
pub fn permission_prompt_primary_detail(tool_name: &str, args: &Value) -> Option<String> {
    if tool_name.starts_with("mcp_") {
        return Some(crate::tool::policy::preview::mcp_args_summary(args));
    }
    match cloud_gated_tool_kind_with_args(tool_name, Some(args)) {
        Some(CloudGatedToolKind::Execute) => command_hint_from_args(args).map(String::from),
        Some(CloudGatedToolKind::Write) => path_hint_from_args(args),
        None => command_hint_from_args(args)
            .map(String::from)
            .or_else(|| path_hint_from_args(args)),
    }
}

/// Human-readable label for the **approval dialog** — the one-line
/// preview shown above the Accept / Reject buttons. Delegates to the
/// shared [`crate::tool::policy::preview::render_preview`] so it matches what
/// the scrollback renders when the tool actually runs.
///
/// Kept separate from [`permission_prompt_primary_detail`] because
/// the two have different contracts: rule matching wants raw args,
/// display wants pretty labels.
pub fn permission_prompt_display_label(tool_name: &str, args: &Value) -> String {
    crate::tool::policy::preview::render_preview(
        tool_name,
        args,
        crate::tool::policy::preview::PreviewStyle::Concise,
        80,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_parses_stringified_json_object() {
        let raw = json!(r#"{"path": "src/x.rs"}"#);
        let v = normalize_llm_function_arguments(&raw);
        assert_eq!(v["path"], "src/x.rs");
    }

    #[test]
    fn normalize_invalid_string_falls_back_to_empty_object() {
        let raw = json!("not json {{{");
        let v = normalize_llm_function_arguments(&raw);
        assert!(v.as_object().map(|o| o.is_empty()).unwrap_or(false));
    }

    #[test]
    fn normalize_passes_through_object() {
        let raw = json!({"path": "p"});
        let v = normalize_llm_function_arguments(&raw);
        assert_eq!(v["path"], "p");
    }

    #[test]
    fn path_hint_reads_path_only() {
        let args = json!({"path": "src/lib.rs"});
        assert_eq!(path_hint_from_args(&args).as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn path_hint_ignores_legacy_file_keys() {
        let args = json!({"file_path": "x.rs", "target_file": "y.rs"});
        assert!(path_hint_from_args(&args).is_none());
    }

    #[test]
    fn command_hint_reads_command_only() {
        let args = json!({"command": "ls -la"});
        assert_eq!(command_hint_from_args(&args), Some("ls -la"));
    }

    #[test]
    fn command_hint_ignores_cmd_key() {
        let args = json!({"cmd": "whoami"});
        assert!(command_hint_from_args(&args).is_none());
    }

    // `permission_prompt_primary_detail` returns the RAW arg for rule
    // matching (rules use starts_with checks). The pretty display
    // label lives in `permission_prompt_display_label` — separating
    // the two avoids silently bypassing deny rules when the display
    // format changes.

    #[test]
    fn normalized_argv_prefix_stops_at_flags_and_shell_meta() {
        assert_eq!(normalized_argv_prefix("cargo test --release"), "cargo test");
        assert_eq!(
            normalized_argv_prefix("npm run deploy:prod -- --foo"),
            "npm run"
        );
        assert_eq!(normalized_argv_prefix("git commit -m 'fix'"), "git commit");
        assert_eq!(normalized_argv_prefix("bash -c 'rm -rf tmp'"), "");
        assert_eq!(normalized_argv_prefix("echo hi > out.txt"), "echo hi");
    }

    #[test]
    fn normalized_argv_prefix_uses_main_command_after_cd_and_env() {
        assert_eq!(
            normalized_argv_prefix("cd rust && cargo test -p astra-cli"),
            "cargo test"
        );
        assert_eq!(
            normalized_argv_prefix("RUST_LOG=debug cargo check --workspace"),
            "cargo check"
        );
        assert_eq!(
            normalized_argv_prefix("cd web && CI=1 npm test -- --runInBand"),
            "npm test"
        );
    }

    #[test]
    fn normalized_argv_prefix_rejects_broad_shell_and_wrapper_rules() {
        assert_eq!(normalized_argv_prefix("python -c 'print(1)'"), "");
        assert_eq!(normalized_argv_prefix("python3 script.py"), "");
        assert_eq!(normalized_argv_prefix("sudo apt install ripgrep"), "");
        assert_eq!(normalized_argv_prefix("cat src/lib.rs"), "");
        assert_eq!(normalized_argv_prefix("UNSAFE=1 npm run build"), "");
        assert_eq!(
            normalized_argv_prefix("NODE_ENV=test npm run build"),
            "npm run"
        );
    }

    #[test]
    fn unsafe_shell_prefix_token_covers_new_interpreters_and_wrappers() {
        for token in ["ash", "nodejs", "deno", "bun", "exec", "busybox", "BusyBox"] {
            assert!(
                is_unsafe_shell_prefix_token(token),
                "{token} should stay blocked as a reusable shell prefix"
            );
        }
    }

    #[test]
    fn subcommand_shape_allows_underscores_without_allowing_flags_or_paths() {
        assert!(looks_like_subcommand("install_deps"));
        assert!(looks_like_subcommand("nextest-run"));
        assert!(!looks_like_subcommand("_hidden"));
        assert!(!looks_like_subcommand("--workspace"));
        assert!(!looks_like_subcommand("src/lib.rs"));
    }

    #[test]
    fn permission_detail_execute_prefers_command_over_path() {
        let args = json!({"command": "ls", "path": "/tmp"});
        assert_eq!(
            permission_prompt_primary_detail("bash", &args).as_deref(),
            Some("ls")
        );
    }

    #[test]
    fn permission_detail_write_uses_path_not_command() {
        let args = json!({"command": "touch x", "path": "/p/x"});
        assert_eq!(
            permission_prompt_primary_detail("write_file", &args).as_deref(),
            Some("/p/x")
        );
    }

    #[test]
    fn permission_detail_read_falls_back_to_path() {
        let args = json!({"path": "/r"});
        assert_eq!(
            permission_prompt_primary_detail("read_file", &args).as_deref(),
            Some("/r")
        );
    }

    #[test]
    fn permission_display_label_uses_rich_preview() {
        let args = json!({"command": "ls -la"});
        assert_eq!(permission_prompt_display_label("bash", &args), "$ ls -la");
        let args = json!({"path": "foo.txt"});
        assert_eq!(
            permission_prompt_display_label("write_file", &args),
            "Writing: foo.txt"
        );
    }

    // ── MCP tool display tests ──

    #[test]
    fn mcp_args_summary_shows_key_values() {
        let args = json!({"query": "hello", "limit": 10});
        let detail = permission_prompt_primary_detail("mcp_search_server", &args).unwrap();
        assert!(detail.contains("query="));
        assert!(detail.contains("hello"));
        assert!(detail.contains("limit="));
    }

    #[test]
    fn mcp_args_summary_empty_args() {
        let detail = permission_prompt_primary_detail("mcp_server_tool", &json!({})).unwrap();
        assert_eq!(detail, "(no arguments)");
    }

    #[test]
    fn mcp_args_summary_truncates_long_values() {
        let long_val = "x".repeat(100);
        let args = json!({"data": long_val});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.len() < 100);
        assert!(detail.contains("…"));
    }

    #[test]
    fn mcp_args_summary_limits_to_3_keys() {
        let args = json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.contains("+2 more"));
    }

    #[test]
    fn mcp_args_summary_long_unicode_no_panic() {
        let long_val = format!("{}end", "数据—".repeat(25));
        let args = json!({"data": long_val});
        let detail = permission_prompt_primary_detail("mcp_server_tool", &args).unwrap();
        assert!(detail.contains("data="));
        assert!(detail.contains('…'));
    }
}
