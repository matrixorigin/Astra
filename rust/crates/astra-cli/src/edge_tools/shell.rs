use super::*;

pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Command semantics — interpret exit codes per-command (inspired by Claude
// Code's commandSemantics.ts). Many commands use non-zero exit codes to convey
// information, not errors. Without this, the model treats grep exit 1 as a
// failure and wastes turns retrying.
// ---------------------------------------------------------------------------

/// Semantic interpretation of a command's exit code.
struct CommandResult {
    is_error: bool,
    /// Optional human-readable note (e.g. "No matches found").
    note: Option<&'static str>,
}

/// Interpret exit code based on the command that produced it.
/// Extracts the *last* command in a pipeline (that's what determines the exit code).
fn interpret_exit_code(command: &str, code: i32) -> CommandResult {
    let base = last_pipeline_command(command);
    match base {
        // grep/rg: 0=matches, 1=no matches, 2+=error
        "grep" | "rg" | "ag" | "ack" => match code {
            0 => CommandResult {
                is_error: false,
                note: None,
            },
            1 => CommandResult {
                is_error: false,
                note: Some("No matches found"),
            },
            _ => CommandResult {
                is_error: true,
                note: None,
            },
        },
        // diff: 0=identical, 1=differences, 2+=error
        "diff" => match code {
            0 | 1 => CommandResult {
                is_error: false,
                note: None,
            },
            _ => CommandResult {
                is_error: true,
                note: None,
            },
        },
        // test/[: 0=true, 1=false, 2+=error
        "test" | "[" => match code {
            0 | 1 => CommandResult {
                is_error: false,
                note: None,
            },
            _ => CommandResult {
                is_error: true,
                note: None,
            },
        },
        // find: 0=ok, 1=partial (some dirs inaccessible), 2+=error
        "find" | "fd" => match code {
            0 | 1 => CommandResult {
                is_error: false,
                note: None,
            },
            _ => CommandResult {
                is_error: true,
                note: None,
            },
        },
        // pkill/pgrep/killall: 0=matched, 1=no match, 2=syntax error, 3=fatal
        "pkill" | "pgrep" | "killall" => match code {
            0 => CommandResult {
                is_error: false,
                note: None,
            },
            1 => CommandResult {
                is_error: false,
                note: Some("No processes matched"),
            },
            _ => CommandResult {
                is_error: true,
                note: None,
            },
        },
        // Default: only 0 is success
        _ => CommandResult {
            is_error: code != 0,
            note: None,
        },
    }
}

/// Extract the base command name from the last segment of a pipeline.
fn last_pipeline_command(command: &str) -> &str {
    let last = command.rsplit('|').next().unwrap_or(command);
    last.split_whitespace().next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Destructive command detection — warn before dangerous operations.
// ---------------------------------------------------------------------------

/// Check if a bash command references file paths outside the sandbox boundary.
///
/// Extracts path-like arguments from common file-access commands (cat, head, tail,
/// less, cp, mv, etc.) and validates them against the sandbox policy. Returns a
/// `SANDBOX_DENIED` error message if any path escapes the boundary.
///
/// This closes the security gap where `read_file("/outside/path")` is blocked by
/// the sandbox but `cat /outside/path` bypasses it entirely.
fn bash_command_segments(command: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut segments = Vec::new();
    let mut segment_start = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut idx = 0usize;

    while idx < chars.len() {
        let (byte_idx, ch) = chars[idx];

        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }

        if ch == '\\' && !in_single_quote {
            escaped = true;
            idx += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            idx += 1;
            continue;
        }

        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            idx += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            // Heredoc start: `<<WORD` / `<<-WORD` (but NOT here-string `<<<`).
            // Without this skip, newlines in the heredoc body would split the
            // body into separate "commands", and tags like `</title>` would be
            // re-scanned as redirections — causing spurious SANDBOX_DENIED.
            if ch == '<'
                && chars.get(idx + 1).is_some_and(|(_, c)| *c == '<')
                && chars.get(idx + 2).is_none_or(|(_, c)| *c != '<')
            {
                let mut hd_idx = idx + 2;
                let strip_tabs = chars.get(hd_idx).is_some_and(|(_, c)| *c == '-');
                if strip_tabs {
                    hd_idx += 1;
                }
                // Emit the command line up to the `<<` operator as its own
                // segment, then resume segmentation after the heredoc
                // terminator. This ensures the heredoc body bytes never
                // leak into any segment where absolute-path scanning
                // would re-flag them.
                let pre_segment = command[segment_start..byte_idx].trim();
                if !pre_segment.is_empty() {
                    segments.push(pre_segment);
                }
                idx = skip_heredoc_body(&chars, hd_idx, strip_tabs);
                segment_start = chars.get(idx).map(|(b, _)| *b).unwrap_or(command.len());
                continue;
            }

            let is_double_separator =
                matches!(ch, '&' | '|') && chars.get(idx + 1).is_some_and(|(_, next)| *next == ch);
            let is_single_separator = matches!(ch, '|' | ';' | '\n' | '\r');

            if is_double_separator || is_single_separator {
                let segment = command[segment_start..byte_idx].trim();
                if !segment.is_empty() {
                    segments.push(segment);
                }

                segment_start = if is_double_separator {
                    let (next_idx, next_ch) = chars[idx + 1];
                    idx += 2;
                    next_idx + next_ch.len_utf8()
                } else {
                    idx += 1;
                    byte_idx + ch.len_utf8()
                };
                continue;
            }
        }

        idx += 1;
    }

    let segment = command[segment_start..].trim();
    if !segment.is_empty() {
        segments.push(segment);
    }

    segments
}

fn check_bash_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
) -> Option<String> {
    let oldpwd = std::env::var_os("OLDPWD").map(std::path::PathBuf::from);
    check_bash_path_boundary_with_oldpwd(policy, command, oldpwd.as_deref())
}

fn check_bash_path_boundary_with_oldpwd(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    if let Some(msg) = check_shell_compound_body_path_boundary(policy, command, oldpwd) {
        return Some(msg);
    }
    if let Some(msg) = check_shell_loop_path_boundary(policy, command, oldpwd) {
        return Some(msg);
    }
    // Split on unquoted shell command separators to check ALL commands, not
    // just the first. Covers: `cmd1 | cmd2`, `cmd1 && cmd2`, `cmd1 ; cmd2`,
    // `cmd1 || cmd2`, and newline-separated commands, while preserving quoted
    // separators and line continuations.
    for segment in bash_command_segments(command) {
        if let Some(msg) = check_single_command_path_boundary(policy, segment, oldpwd) {
            return Some(msg);
        }
    }
    None
}

fn find_powershell_program() -> Option<&'static str> {
    for candidate in ["pwsh", "powershell"] {
        let status = Command::new(candidate)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "$PSVersionTable.PSVersion.Major",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok() {
            return Some(candidate);
        }
    }
    None
}

/// Check if a PowerShell command references file paths outside the sandbox boundary.
///
/// This is intentionally conservative and only inspects common cmdlets whose
/// first positional argument is usually a file path.
fn check_powershell_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
) -> Option<String> {
    let commands = command
        .split(&['|', ';', '\n', '\r'][..])
        .flat_map(|segment| segment.split("&&"))
        .flat_map(|segment| segment.split("||"));

    for segment in commands {
        let parts: Vec<&str> = segment.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let base = parts[0].to_ascii_lowercase();
        let is_file_access_cmd = matches!(
            base.as_str(),
            "get-content"
                | "set-content"
                | "add-content"
                | "copy-item"
                | "move-item"
                | "remove-item"
                | "get-item"
                | "test-path"
                | "resolve-path"
                | "rename-item"
        );
        if !is_file_access_cmd {
            continue;
        }

        for arg in &parts[1..] {
            if arg.starts_with('-') || arg.starts_with('$') {
                continue;
            }
            let trimmed = arg.trim_matches(|c| c == '"' || c == '\'' || c == '`');
            if trimmed.is_empty()
                || trimmed.contains('*')
                || trimmed.contains('?')
                || trimmed.starts_with("http://")
                || trimmed.starts_with("https://")
            {
                continue;
            }

            let resolved = if trimmed.starts_with('/') || trimmed.contains(":\\") {
                PathBuf::from(trimmed)
            } else {
                policy.project_root.join(trimmed)
            };
            let path_str = resolved.to_string_lossy();
            if let Err(e) = validate_path(policy, &path_str)
                && e.is_boundary_violation()
            {
                return Some(format!(
                    "{}The command references '{}' which is outside the project directory '{}'. \
                     Ask the user for permission before accessing files outside the project.",
                    super::SANDBOX_DENIED_PREFIX,
                    trimmed,
                    policy.project_root.display(),
                ));
            }
        }
    }
    None
}

fn shell_tokenize_like_bash(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_double_quote = false;
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next() {
                    if next == '\n' || next == '\r' {
                        continue;
                    }
                    if let Some(escaped) = escaped_shell_analysis_char(next) {
                        current.push(escaped);
                    } else {
                        current.push(next);
                    }
                }
            }
            c if c.is_whitespace() && !in_double_quote && !in_single_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

#[derive(Clone, Debug)]
struct ShellTokenSpan {
    text: String,
    start: usize,
    end: usize,
}

fn shell_tokenize_with_control_spans(input: &str) -> Vec<ShellTokenSpan> {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_start = None::<usize>;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut idx = 0usize;

    let flush_current = |tokens: &mut Vec<ShellTokenSpan>,
                         current: &mut String,
                         current_start: &mut Option<usize>,
                         end: usize| {
        if let Some(start) = current_start.take()
            && !current.is_empty()
        {
            tokens.push(ShellTokenSpan {
                text: std::mem::take(current),
                start,
                end,
            });
        } else {
            current.clear();
        }
    };

    while idx < chars.len() {
        let (byte_idx, ch) = chars[idx];

        if escaped {
            escaped = false;
            current.push(ch);
            idx += 1;
            continue;
        }

        if ch == '\\' && !in_single_quote {
            current_start.get_or_insert(byte_idx);
            escaped = true;
            idx += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            current_start.get_or_insert(byte_idx);
            in_single_quote = !in_single_quote;
            idx += 1;
            continue;
        }

        if ch == '"' && !in_single_quote {
            current_start.get_or_insert(byte_idx);
            in_double_quote = !in_double_quote;
            idx += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            if ch.is_whitespace() {
                flush_current(&mut tokens, &mut current, &mut current_start, byte_idx);
                idx += 1;
                continue;
            }

            if ch == '(' && current.is_empty() {
                tokens.push(ShellTokenSpan {
                    text: "(".to_string(),
                    start: byte_idx,
                    end: byte_idx + ch.len_utf8(),
                });
                idx += 1;
                continue;
            }

            if ch == '{' && current.ends_with("()") {
                flush_current(&mut tokens, &mut current, &mut current_start, byte_idx);
                tokens.push(ShellTokenSpan {
                    text: "{".to_string(),
                    start: byte_idx,
                    end: byte_idx + ch.len_utf8(),
                });
                idx += 1;
                continue;
            }

            if matches!(ch, ';' | '\n' | '\r') {
                flush_current(&mut tokens, &mut current, &mut current_start, byte_idx);
                tokens.push(ShellTokenSpan {
                    text: ";".to_string(),
                    start: byte_idx,
                    end: byte_idx + ch.len_utf8(),
                });
                idx += 1;
                continue;
            }

            if matches!(ch, '|' | '&') {
                flush_current(&mut tokens, &mut current, &mut current_start, byte_idx);
                let (text, consumed, end) =
                    if chars.get(idx + 1).is_some_and(|(_, next)| *next == ch) {
                        let (next_idx, next_ch) = chars[idx + 1];
                        (format!("{ch}{ch}"), 2usize, next_idx + next_ch.len_utf8())
                    } else {
                        (ch.to_string(), 1usize, byte_idx + ch.len_utf8())
                    };
                tokens.push(ShellTokenSpan {
                    text,
                    start: byte_idx,
                    end,
                });
                idx += consumed;
                continue;
            }
        }

        current_start.get_or_insert(byte_idx);
        current.push(ch);
        idx += 1;
    }

    flush_current(&mut tokens, &mut current, &mut current_start, input.len());
    tokens
}

fn is_shell_assignment_token(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn first_segment_subcommand(parts: &[String]) -> Option<&str> {
    let mut idx = 0usize;
    while parts
        .get(idx)
        .is_some_and(|part| is_shell_assignment_token(part))
    {
        idx += 1;
    }

    let mut base = parts.get(idx)?.as_str();
    if matches!(base, "command" | "builtin") {
        idx += 1;
        while parts
            .get(idx)
            .is_some_and(|part| is_shell_assignment_token(part))
        {
            idx += 1;
        }
        base = parts.get(idx)?.as_str();
    } else if base == "env" {
        idx += 1;
        while let Some(part) = parts.get(idx) {
            let token = part.as_str();
            if token.starts_with('-') || is_shell_assignment_token(token) {
                idx += 1;
                continue;
            }
            base = token;
            break;
        }
        if idx >= parts.len() {
            return None;
        }
    }

    Some(base.rsplit('/').next().unwrap_or(base))
}

const ESCAPED_SHELL_DOLLAR: char = '\u{E000}';
const ESCAPED_SHELL_TILDE: char = '\u{E001}';
const ESCAPED_SHELL_OPEN_BRACE: char = '\u{E002}';
const ESCAPED_SHELL_CLOSE_BRACE: char = '\u{E003}';

fn escaped_shell_analysis_char(ch: char) -> Option<char> {
    match ch {
        '$' => Some(ESCAPED_SHELL_DOLLAR),
        '~' => Some(ESCAPED_SHELL_TILDE),
        '{' => Some(ESCAPED_SHELL_OPEN_BRACE),
        '}' => Some(ESCAPED_SHELL_CLOSE_BRACE),
        _ => None,
    }
}

fn restore_escaped_shell_analysis_chars(arg: &str) -> String {
    arg.chars()
        .map(|ch| match ch {
            ESCAPED_SHELL_DOLLAR => '$',
            ESCAPED_SHELL_TILDE => '~',
            ESCAPED_SHELL_OPEN_BRACE => '{',
            ESCAPED_SHELL_CLOSE_BRACE => '}',
            _ => ch,
        })
        .collect()
}

fn shell_short_flag_cluster(flag: &str) -> Option<&str> {
    let rest = flag.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') || !rest.chars().all(|ch| ch.is_ascii_alphabetic())
    {
        return None;
    }
    Some(rest)
}

fn is_nested_shell_c_flag(flag: &str) -> bool {
    shell_short_flag_cluster(flag).is_some_and(|rest| rest.contains('c'))
}

fn is_shell_read_from_stdin_flag(flag: &str) -> bool {
    shell_short_flag_cluster(flag).is_some_and(|rest| rest.contains('s'))
}

enum ShellFlagValueKind {
    NestedCommand,
    InitCommand,
    Path,
    Other,
}

fn shell_flag_value_kind(flag: &str) -> Option<ShellFlagValueKind> {
    match flag {
        "--command" => Some(ShellFlagValueKind::NestedCommand),
        "-C" | "--init-command" => Some(ShellFlagValueKind::InitCommand),
        "--rcfile" | "--init-file" => Some(ShellFlagValueKind::Path),
        "-o" | "+o" | "-O" | "+O" => Some(ShellFlagValueKind::Other),
        _ => None,
    }
}

fn validate_command_path_arg(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    if let Some(msg) = validate_brace_expansion_path_arg(policy, arg, oldpwd) {
        return Some(msg);
    }

    validate_plain_command_path_arg(policy, arg, oldpwd)
}

fn validate_plain_command_path_arg(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let literal_arg = restore_escaped_shell_analysis_chars(arg);
    if let Some(kind) = unresolved_static_dir_reference_kind(policy, arg, oldpwd) {
        return Some(format!(
            "{}The command references '{}' using {} which cannot be statically validated against the project directory '{}'. Ask the user for permission before accessing files outside the project.",
            super::SANDBOX_DENIED_PREFIX,
            literal_arg,
            kind,
            policy.project_root.display(),
        ));
    }

    let resolved = if literal_arg.starts_with('/') {
        std::path::PathBuf::from(&literal_arg)
    } else if let Some(expanded) = expand_static_dir_reference(policy, arg, oldpwd) {
        expanded
    } else {
        policy.project_root.join(&literal_arg)
    };
    let path_str = resolved.to_string_lossy();
    if let Err(e) = validate_path(policy, &path_str)
        && e.is_boundary_violation()
    {
        return Some(format!(
            "{}The command references '{}' which is outside the project directory '{}'. \
             Ask the user for permission before accessing files outside the project.",
            super::SANDBOX_DENIED_PREFIX,
            literal_arg,
            policy.project_root.display(),
        ));
    }
    None
}

fn validate_brace_expansion_path_arg(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    match brace_expansion_candidates(arg)? {
        BraceExpansionCandidates::RequiresReview => {
            Some(brace_expansion_boundary_review_message(policy, arg))
        }
        BraceExpansionCandidates::Expanded(candidates) => {
            for candidate in candidates {
                if validate_plain_command_path_arg(policy, &candidate, oldpwd).is_some() {
                    return Some(brace_expansion_boundary_review_message(policy, arg));
                }
            }
            None
        }
    }
}

fn brace_expansion_boundary_review_message(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
) -> String {
    format!(
        "{}The command references '{}' using shell brace expansion, which may fan out to multiple paths that cannot be statically validated against the project directory '{}'. Ask the user for permission before accessing files outside the project.",
        super::SANDBOX_DENIED_PREFIX,
        arg,
        policy.project_root.display(),
    )
}

fn check_shell_interpreter_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    parts: &[String],
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let mut idx = 1usize;
    while idx < parts.len() {
        let arg = parts[idx].as_str();

        if let Some(inner) = arg.strip_prefix("--command=") {
            return check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd);
        }
        if let Some(inner) = arg.strip_prefix("--init-command=") {
            if let Some(msg) = check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd) {
                return Some(msg);
            }
            idx += 1;
            continue;
        }
        if let Some(path) = arg
            .strip_prefix("--rcfile=")
            .or_else(|| arg.strip_prefix("--init-file="))
        {
            if let Some(msg) = validate_command_path_arg(policy, path, oldpwd) {
                return Some(msg);
            }
            idx += 1;
            continue;
        }

        if arg == "--" {
            return parts
                .get(idx + 1)
                .and_then(|script| validate_command_path_arg(policy, script, oldpwd));
        }

        if is_nested_shell_c_flag(arg) {
            return parts
                .get(idx + 1)
                .and_then(|inner| check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd));
        }

        if is_shell_read_from_stdin_flag(arg) {
            return None;
        }

        match shell_flag_value_kind(arg) {
            Some(ShellFlagValueKind::NestedCommand) => {
                return parts
                    .get(idx + 1)
                    .and_then(|inner| check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd));
            }
            Some(ShellFlagValueKind::InitCommand) => {
                if let Some(inner) = parts.get(idx + 1)
                    && let Some(msg) = check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd)
                {
                    return Some(msg);
                }
                idx += 2;
                continue;
            }
            Some(ShellFlagValueKind::Path) => {
                if let Some(path) = parts.get(idx + 1)
                    && let Some(msg) = validate_command_path_arg(policy, path, oldpwd)
                {
                    return Some(msg);
                }
                idx += 2;
                continue;
            }
            Some(ShellFlagValueKind::Other) => {
                idx += 2;
                continue;
            }
            None => {}
        }

        if arg.starts_with('-') || arg.starts_with('+') {
            idx += 1;
            continue;
        }

        return validate_command_path_arg(policy, arg, oldpwd);
    }

    None
}

fn expand_static_dir_reference(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    expand_home_dir_reference(arg)
        .or_else(|| expand_project_dir_reference(policy, arg))
        .or_else(|| expand_oldpwd_dir_reference(arg, oldpwd))
}

fn expand_home_dir_reference(arg: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    if matches!(arg, "~" | "$HOME" | "${HOME}") {
        return Some(home);
    }
    let suffix = arg
        .strip_prefix("~/")
        .or_else(|| arg.strip_prefix("$HOME/"))
        .or_else(|| arg.strip_prefix("${HOME}/"))?;
    Some(home.join(suffix))
}

fn expand_project_dir_reference(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
) -> Option<std::path::PathBuf> {
    if matches!(arg, "$PWD" | "${PWD}" | "~+") {
        return Some(policy.project_root.clone());
    }
    let suffix = arg
        .strip_prefix("$PWD/")
        .or_else(|| arg.strip_prefix("${PWD}/"))
        .or_else(|| arg.strip_prefix("~+/"))?;
    Some(policy.project_root.join(suffix))
}

fn expand_oldpwd_dir_reference(
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let oldpwd = oldpwd?;
    if matches!(arg, "$OLDPWD" | "${OLDPWD}" | "~-") {
        return Some(oldpwd.to_path_buf());
    }
    let suffix = arg
        .strip_prefix("$OLDPWD/")
        .or_else(|| arg.strip_prefix("${OLDPWD}/"))
        .or_else(|| arg.strip_prefix("~-/"))?;
    Some(oldpwd.join(suffix))
}

fn unresolved_static_dir_reference_kind(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<&'static str> {
    if is_named_tilde_user_reference(arg) {
        return Some("~user home-directory expansion");
    }

    (is_complex_dir_parameter_reference(arg, "HOME")
        || is_complex_dir_parameter_reference(arg, "PWD")
        || is_complex_dir_parameter_reference(arg, "OLDPWD"))
    .then_some("shell parameter expansion from a directory anchor")
    .or_else(|| {
        (expand_static_dir_reference(policy, arg, oldpwd).is_none()
            && contains_unresolved_shell_variable(arg))
        .then_some("shell variable expansion")
    })
}

fn is_named_tilde_user_reference(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('~') else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') || rest.starts_with('+') || rest.starts_with('-') {
        return false;
    }

    let login = rest.split('/').next().unwrap_or_default();
    !login.is_empty()
        && login
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn is_complex_dir_parameter_reference(arg: &str, name: &str) -> bool {
    let exact = format!("${{{name}}}");
    if arg == exact || arg.starts_with(&format!("{exact}/")) {
        return false;
    }
    if let Some(rest) = arg.strip_prefix(&exact) {
        return !rest.is_empty();
    }

    let Some(rest) = arg.strip_prefix(&format!("${{{name}")) else {
        return false;
    };

    matches!(
        rest.as_bytes().first().copied(),
        Some(b':' | b'-' | b'=' | b'?' | b'+' | b'%' | b'#' | b'/' | b'^' | b',')
    )
}

fn contains_unresolved_shell_variable(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] != b'$' {
            idx += 1;
            continue;
        }
        match bytes.get(idx + 1).copied() {
            Some(b'{') => return true,
            Some(next)
                if next.is_ascii_alphanumeric()
                    || matches!(next, b'_' | b'*' | b'@' | b'#' | b'?' | b'-' | b'$' | b'!') =>
            {
                return true;
            }
            _ => idx += 1,
        }
    }
    false
}

enum BraceExpansionCandidates {
    Expanded(Vec<String>),
    RequiresReview,
}

fn brace_expansion_candidates(arg: &str) -> Option<BraceExpansionCandidates> {
    let mut start = None;
    let mut end = None;
    let mut depth = 0usize;
    let mut saw_comma = false;

    for (idx, ch) in arg.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                } else {
                    return Some(BraceExpansionCandidates::RequiresReview);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    return Some(BraceExpansionCandidates::RequiresReview);
                }
                depth -= 1;
                if depth == 0 {
                    end = Some(idx);
                    break;
                }
            }
            ',' if depth == 1 => saw_comma = true,
            _ => {}
        }
    }

    let (start, end) = match (start, end) {
        (Some(start), Some(end)) if saw_comma => (start, end),
        _ => return None,
    };

    if arg[end + 1..].contains('{') || arg[end + 1..].contains('}') {
        return Some(BraceExpansionCandidates::RequiresReview);
    }

    let prefix = &arg[..start];
    let suffix = &arg[end + 1..];
    let inner = &arg[start + 1..end];
    if inner.contains('{') || inner.contains('}') {
        return Some(BraceExpansionCandidates::RequiresReview);
    }

    Some(BraceExpansionCandidates::Expanded(
        inner
            .split(',')
            .map(|part| format!("{prefix}{part}{suffix}"))
            .collect(),
    ))
}

fn process_substitution_commands(command: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut commands = Vec::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut idx = 0usize;

    while idx < chars.len() {
        let (_, ch) = chars[idx];

        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }

        if ch == '\\' && !in_single_quote {
            escaped = true;
            idx += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            idx += 1;
            continue;
        }

        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            idx += 1;
            continue;
        }

        if !in_single_quote
            && !in_double_quote
            && matches!(ch, '<' | '>')
            && chars.get(idx + 1).is_some_and(|(_, next)| *next == '(')
        {
            let (open_idx, open_ch) = chars[idx + 1];
            let inner_start = open_idx + open_ch.len_utf8();
            idx += 2;

            let mut depth = 1usize;
            let mut inner_in_single_quote = false;
            let mut inner_in_double_quote = false;
            let mut inner_escaped = false;

            while idx < chars.len() {
                let (byte_idx, inner_ch) = chars[idx];

                if inner_escaped {
                    inner_escaped = false;
                    idx += 1;
                    continue;
                }

                if inner_ch == '\\' && !inner_in_single_quote {
                    inner_escaped = true;
                    idx += 1;
                    continue;
                }

                if inner_ch == '\'' && !inner_in_double_quote {
                    inner_in_single_quote = !inner_in_single_quote;
                    idx += 1;
                    continue;
                }

                if inner_ch == '"' && !inner_in_single_quote {
                    inner_in_double_quote = !inner_in_double_quote;
                    idx += 1;
                    continue;
                }

                if !inner_in_single_quote && !inner_in_double_quote {
                    match inner_ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                let inner = command[inner_start..byte_idx].trim();
                                if !inner.is_empty() {
                                    commands.push(inner);
                                }
                                idx += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }

                idx += 1;
            }
            continue;
        }

        idx += 1;
    }

    commands
}

fn check_redirection_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut idx = 0usize;

    while idx < chars.len() {
        let (_, ch) = chars[idx];

        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }

        if ch == '\\' && !in_single_quote {
            escaped = true;
            idx += 1;
            continue;
        }

        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            idx += 1;
            continue;
        }

        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            idx += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && matches!(ch, '<' | '>') {
            if chars.get(idx + 1).is_some_and(|(_, next)| *next == '(') {
                idx += 1;
                continue;
            }

            let (next_idx, consumes_path) = redirection_operator_details(&chars, idx);
            // Heredoc (`<<WORD` or `<<-WORD`, but NOT here-string `<<<word`):
            // skip the heredoc body so that `<`/`>` inside HTML / XML / template
            // payloads aren't misparsed as further redirection operators.
            let is_heredoc = ch == '<'
                && (next_idx - idx) == 2
                && chars.get(idx + 1).is_some_and(|(_, c)| *c == '<')
                && chars.get(idx + 2).is_none_or(|(_, c)| *c != '<');
            if is_heredoc {
                let mut hd_idx = next_idx;
                let strip_tabs = chars.get(hd_idx).is_some_and(|(_, c)| *c == '-');
                if strip_tabs {
                    hd_idx += 1;
                }
                idx = skip_heredoc_body(&chars, hd_idx, strip_tabs);
                continue;
            }
            idx = next_idx;
            if !consumes_path {
                continue;
            }

            while idx < chars.len() && chars[idx].1.is_whitespace() {
                idx += 1;
            }
            if idx >= chars.len() {
                break;
            }

            let target_start = chars[idx].0;
            let mut target_in_single_quote = false;
            let mut target_in_double_quote = false;
            let mut target_escaped = false;

            while idx < chars.len() {
                let (byte_idx, target_ch) = chars[idx];

                if target_escaped {
                    target_escaped = false;
                    idx += 1;
                    continue;
                }

                if target_ch == '\\' && !target_in_single_quote {
                    target_escaped = true;
                    idx += 1;
                    continue;
                }

                if target_ch == '\'' && !target_in_double_quote {
                    target_in_single_quote = !target_in_single_quote;
                    idx += 1;
                    continue;
                }

                if target_ch == '"' && !target_in_single_quote {
                    target_in_double_quote = !target_in_double_quote;
                    idx += 1;
                    continue;
                }

                if !target_in_single_quote
                    && !target_in_double_quote
                    && (target_ch.is_whitespace()
                        || matches!(target_ch, '|' | ';' | '&' | '\n' | '\r' | '<' | '>'))
                {
                    let raw_target = command[target_start..byte_idx].trim();
                    if let Some(target) = shell_tokenize_like_bash(raw_target).first()
                        && let Some(msg) = validate_command_path_arg(policy, target, oldpwd)
                    {
                        return Some(msg);
                    }
                    break;
                }

                idx += 1;
            }

            if idx >= chars.len() {
                let raw_target = command[target_start..].trim();
                if let Some(target) = shell_tokenize_like_bash(raw_target).first()
                    && let Some(msg) = validate_command_path_arg(policy, target, oldpwd)
                {
                    return Some(msg);
                }
                break;
            }

            continue;
        }

        idx += 1;
    }

    None
}

fn redirection_operator_details(chars: &[(usize, char)], idx: usize) -> (usize, bool) {
    match (
        chars[idx].1,
        chars.get(idx + 1).map(|(_, ch)| *ch),
        chars.get(idx + 2).map(|(_, ch)| *ch),
    ) {
        ('<', Some('<'), Some('<')) => (idx + 3, false),
        ('<', Some('<'), _) => (idx + 2, false),
        ('>', Some('>'), _) => (idx + 2, true),
        ('<', Some('>'), _) => (idx + 2, true),
        ('>', Some('|'), _) => (idx + 2, true),
        ('<', Some('&'), _) => (idx + 2, false),
        ('>', Some('&'), _) => (idx + 2, true),
        _ => (idx + 1, true),
    }
}

/// Skip past a heredoc body (`<<WORD` / `<<-WORD`, quoted or not) so that
/// any `<` / `>` / shell meta inside the body can't be misparsed as further
/// redirections. Called with `start_idx` pointing just after `<<` (or `<<-`).
///
/// Heredoc semantics we honour:
/// * The delimiter word may be quoted (`'WORD'` / `"WORD"`) or unquoted; quotes
///   or a leading backslash merely suppress expansion inside the body — here
///   we only need the literal terminator.
/// * `<<-WORD` strips leading tabs from terminator-line comparison.
/// * Body runs from the next newline up to (and including) the line that
///   equals the delimiter. If EOF is reached with no terminator we bail to
///   end-of-input — further redirection scanning would be unsound anyway.
fn skip_heredoc_body(chars: &[(usize, char)], start_idx: usize, strip_tabs: bool) -> usize {
    let mut idx = start_idx;

    // Skip any whitespace between `<<` and the delimiter word.
    while idx < chars.len() && matches!(chars[idx].1, ' ' | '\t') {
        idx += 1;
    }

    // Parse delimiter word (optionally quoted). Backslash escapes a single char.
    let mut delim = String::new();
    let quote = match chars.get(idx).map(|(_, c)| *c) {
        Some('\'') => {
            idx += 1;
            Some('\'')
        }
        Some('"') => {
            idx += 1;
            Some('"')
        }
        _ => None,
    };
    while idx < chars.len() {
        let c = chars[idx].1;
        match quote {
            Some(q) => {
                if c == q {
                    idx += 1;
                    break;
                }
                delim.push(c);
                idx += 1;
            }
            None => {
                if c.is_whitespace() || matches!(c, '|' | ';' | '&' | '<' | '>') {
                    break;
                }
                if c == '\\' {
                    idx += 1;
                    if let Some((_, next)) = chars.get(idx) {
                        delim.push(*next);
                        idx += 1;
                    }
                } else {
                    delim.push(c);
                    idx += 1;
                }
            }
        }
    }

    if delim.is_empty() {
        // Malformed `<<` with no delimiter — refuse to parse the rest rather
        // than risk misclassifying body content as redirections.
        return chars.len();
    }

    // Advance to the newline that starts the body. Anything between the
    // delimiter word and that newline is either whitespace, another
    // redirection target (e.g. `<< EOF > out.txt`), or a following command.
    // We conservatively let it pass through — the outer validator will pick
    // up `> out.txt` on the next iteration, but since we already `continue`d
    // we need the caller to resume at the newline.
    while idx < chars.len() && chars[idx].1 != '\n' {
        idx += 1;
    }
    if idx < chars.len() {
        idx += 1; // consume newline → body starts
    }

    // Scan body line-by-line for a line matching `delim` exactly
    // (after optional leading-tab stripping for `<<-`).
    while idx < chars.len() {
        let line_start = idx;
        while idx < chars.len() && chars[idx].1 != '\n' {
            idx += 1;
        }
        let line_end = idx;
        let mut line: String = chars[line_start..line_end]
            .iter()
            .map(|(_, c)| *c)
            .collect();
        if strip_tabs {
            line = line.trim_start_matches('\t').to_string();
        }
        if line == delim {
            if idx < chars.len() {
                idx += 1; // consume terminating newline
            }
            return idx;
        }
        if idx < chars.len() {
            idx += 1; // consume newline and continue
        }
    }

    // EOF without terminator — bail to end so the outer scanner stops.
    chars.len()
}

fn is_shell_interpreter_command(base: &str) -> bool {
    matches!(base, "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish")
}

fn is_boundary_sensitive_file_access_command(base: &str) -> bool {
    matches!(
        base,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "tac"
            | "nl"
            | "cp"
            | "mv"
            | "rm"
            | "ln"
            | "install"
            | "stat"
            | "file"
            | "wc"
            | "md5sum"
            | "sha256sum"
            | "sha1sum"
            | "readlink"
            | "realpath"
            | "diff"
            | "sort"
            | "awk"
            | "sed"
            | "tee"
            | "cmp"
            | "comm"
            | "join"
            | "cut"
            | "paste"
            | "uniq"
            | "source"
            | "."
    )
}

fn subcommand_requires_boundary_review(base: &str) -> bool {
    is_boundary_sensitive_file_access_command(base) || is_shell_interpreter_command(base)
}

fn check_shell_compound_body_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let tokens = shell_tokenize_with_control_spans(command);
    let mut idx = 0usize;
    while idx < tokens.len() {
        match tokens[idx].text.as_str() {
            "if" => {
                let Some((then_idx, else_idx, fi_idx)) = if_body_token_indices(&tokens, idx) else {
                    idx += 1;
                    continue;
                };
                if let Some(msg) = check_shell_body_span_path_boundary(
                    policy,
                    command,
                    tokens[then_idx].end,
                    else_idx
                        .map(|body_idx| tokens[body_idx].start)
                        .unwrap_or(tokens[fi_idx].start),
                    oldpwd,
                ) {
                    return Some(msg);
                }
                if let Some(else_idx) = else_idx
                    && let Some(msg) = check_shell_body_span_path_boundary(
                        policy,
                        command,
                        tokens[else_idx].end,
                        tokens[fi_idx].start,
                        oldpwd,
                    )
                {
                    return Some(msg);
                }
                idx = fi_idx + 1;
            }
            "case" => {
                let Some(esac_idx) = case_esac_token_index(&tokens, idx) else {
                    idx += 1;
                    continue;
                };
                if let Some(msg) = check_case_clause_bodies_path_boundary(
                    policy, command, &tokens, idx, esac_idx, oldpwd,
                ) {
                    return Some(msg);
                }
                idx = esac_idx + 1;
            }
            "{" => {
                let Some(close_idx) = brace_group_close_token_index(&tokens, idx) else {
                    idx += 1;
                    continue;
                };
                if let Some(msg) = check_shell_body_span_path_boundary(
                    policy,
                    command,
                    tokens[idx].end,
                    tokens[close_idx].start,
                    oldpwd,
                ) {
                    return Some(msg);
                }
                idx = close_idx + 1;
            }
            "(" => {
                let Some(close_byte_idx) =
                    subshell_group_close_byte_index(command, tokens[idx].start)
                else {
                    idx += 1;
                    continue;
                };
                if let Some(msg) = check_shell_body_span_path_boundary(
                    policy,
                    command,
                    tokens[idx].end,
                    close_byte_idx,
                    oldpwd,
                ) {
                    return Some(msg);
                }
                while idx < tokens.len() && tokens[idx].start <= close_byte_idx {
                    idx += 1;
                }
            }
            _ => idx += 1,
        }
    }
    None
}

fn check_shell_body_span_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    body_start: usize,
    body_end: usize,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let body = command[body_start..body_end].trim();
    (!body.is_empty())
        .then(|| check_bash_path_boundary_with_oldpwd(policy, body, oldpwd))
        .flatten()
}

fn if_body_token_indices(
    tokens: &[ShellTokenSpan],
    if_idx: usize,
) -> Option<(usize, Option<usize>, usize)> {
    let then_idx = ((if_idx + 1)..tokens.len()).find(|idx| tokens[*idx].text.as_str() == "then")?;
    let mut nested_ifs = 0usize;
    let mut else_idx = None;
    for idx in (then_idx + 1)..tokens.len() {
        match tokens[idx].text.as_str() {
            "if" => nested_ifs += 1,
            "fi" if nested_ifs == 0 => return Some((then_idx, else_idx, idx)),
            "fi" => nested_ifs -= 1,
            "else" if nested_ifs == 0 && else_idx.is_none() => else_idx = Some(idx),
            _ => {}
        }
    }
    None
}

fn brace_group_close_token_index(tokens: &[ShellTokenSpan], open_idx: usize) -> Option<usize> {
    let mut nested_groups = 0usize;
    for idx in (open_idx + 1)..tokens.len() {
        match tokens[idx].text.as_str() {
            "{" => nested_groups += 1,
            "}" if nested_groups == 0 => return Some(idx),
            "}" => nested_groups -= 1,
            _ => {}
        }
    }
    None
}

fn subshell_group_close_byte_index(command: &str, open_byte_idx: usize) -> Option<usize> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let open_idx = chars
        .iter()
        .position(|(byte_idx, ch)| *byte_idx == open_byte_idx && *ch == '(')?;
    let mut depth = 1usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for idx in (open_idx + 1)..chars.len() {
        let (byte_idx, ch) = chars[idx];

        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if ch == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if in_single_quote || in_double_quote {
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(byte_idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn case_esac_token_index(tokens: &[ShellTokenSpan], case_idx: usize) -> Option<usize> {
    let mut nested_cases = 0usize;
    for idx in (case_idx + 1)..tokens.len() {
        match tokens[idx].text.as_str() {
            "case" => nested_cases += 1,
            "esac" if nested_cases == 0 => return Some(idx),
            "esac" => nested_cases -= 1,
            _ => {}
        }
    }
    None
}

fn check_case_clause_bodies_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    tokens: &[ShellTokenSpan],
    case_idx: usize,
    esac_idx: usize,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let in_idx = ((case_idx + 1)..esac_idx).find(|idx| tokens[*idx].text.as_str() == "in")?;
    let mut idx = in_idx + 1;
    while idx < esac_idx {
        while idx < esac_idx && tokens[idx].text.as_str() == ";" {
            idx += 1;
        }
        if idx >= esac_idx {
            break;
        }

        let pattern_end_idx = (idx..esac_idx)
            .find(|body_idx| case_pattern_token_closes_clause(tokens[*body_idx].text.as_str()))?;
        let clause_end = case_clause_terminator(tokens, pattern_end_idx + 1, esac_idx)
            .map(|(terminator_idx, _len)| tokens[terminator_idx].start)
            .unwrap_or(tokens[esac_idx].start);
        if let Some(msg) = check_shell_body_span_path_boundary(
            policy,
            command,
            tokens[pattern_end_idx].end,
            clause_end,
            oldpwd,
        ) {
            return Some(msg);
        }
        idx = case_clause_terminator(tokens, pattern_end_idx + 1, esac_idx)
            .map(|(terminator_idx, len)| terminator_idx + len)
            .unwrap_or(esac_idx);
    }
    None
}

fn case_pattern_token_closes_clause(token: &str) -> bool {
    token == ")" || token.ends_with(')')
}

fn case_clause_terminator(
    tokens: &[ShellTokenSpan],
    start_idx: usize,
    esac_idx: usize,
) -> Option<(usize, usize)> {
    let mut nested_cases = 0usize;
    let mut idx = start_idx;
    while idx < esac_idx {
        match tokens[idx].text.as_str() {
            "case" => nested_cases += 1,
            "esac" if nested_cases > 0 => nested_cases -= 1,
            ";" if nested_cases == 0 => {
                if idx + 2 < esac_idx
                    && tokens[idx + 1].text.as_str() == ";"
                    && tokens[idx + 2].text.as_str() == "&"
                {
                    return Some((idx, 3));
                }
                if idx + 1 < esac_idx && matches!(tokens[idx + 1].text.as_str(), ";" | "&") {
                    return Some((idx, 2));
                }
            }
            _ => {}
        }
        idx += 1;
    }
    None
}

fn check_shell_loop_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let tokens = shell_tokenize_with_control_spans(command);
    let mut idx = 0usize;
    while idx < tokens.len() {
        let loop_kind = match tokens[idx].text.as_str() {
            "while" => ShellLoopKind::WhileRead,
            "for" => ShellLoopKind::ForIn,
            _ => {
                idx += 1;
                continue;
            }
        };

        let Some((do_idx, done_idx)) = shell_loop_body_token_indices(&tokens, idx) else {
            idx += 1;
            continue;
        };

        let body = command[tokens[do_idx].end..tokens[done_idx].start].trim();
        let loop_fanout_kind = match loop_kind {
            ShellLoopKind::WhileRead
                if tokens[idx + 1..do_idx]
                    .iter()
                    .any(|token| token.text.as_str() == "read") =>
            {
                Some(loop_kind)
            }
            ShellLoopKind::ForIn
                if tokens[idx + 1..do_idx]
                    .iter()
                    .any(|token| token.text.as_str() == "in") =>
            {
                Some(loop_kind)
            }
            _ => None,
        };
        let fanout_subcommand = loop_fanout_kind.and_then(|kind| {
            shell_loop_body_subcommand_requires_boundary_review(body)
                .map(|subcommand| (kind, subcommand))
        });

        if let Some(msg) = (!body.is_empty())
            .then(|| check_bash_path_boundary_with_oldpwd(policy, body, oldpwd))
            .flatten()
        {
            if let Some((kind, subcommand)) = fanout_subcommand.as_ref()
                && msg.contains("shell variable expansion")
            {
                return Some(shell_loop_fanout_review_message(policy, *kind, subcommand));
            }
            return Some(msg);
        }
        if let Some((kind, subcommand)) = fanout_subcommand {
            return Some(shell_loop_fanout_review_message(policy, kind, &subcommand));
        }
        idx = done_idx + 1;
    }
    None
}

fn shell_loop_fanout_review_message(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    loop_kind: ShellLoopKind,
    subcommand: &str,
) -> String {
    format!(
        "{}The command uses `{loop_kind} {subcommand}` so file paths may be supplied from shell loop iterations and cannot be statically validated against the project directory '{}'. Ask the user for permission before using shell loop fan-out with file-access or shell commands.",
        super::SANDBOX_DENIED_PREFIX,
        policy.project_root.display(),
    )
}

fn shell_loop_body_token_indices(
    tokens: &[ShellTokenSpan],
    loop_idx: usize,
) -> Option<(usize, usize)> {
    let do_idx = ((loop_idx + 1)..tokens.len()).find(|idx| tokens[*idx].text.as_str() == "do")?;
    let mut nested_loops = 0usize;
    for idx in (do_idx + 1)..tokens.len() {
        match tokens[idx].text.as_str() {
            "while" | "for" => nested_loops += 1,
            "done" if nested_loops == 0 => return Some((do_idx, idx)),
            "done" => nested_loops -= 1,
            _ => {}
        }
    }
    None
}

fn shell_loop_body_subcommand_requires_boundary_review(body: &str) -> Option<String> {
    for segment in bash_command_segments(body) {
        let parts = shell_tokenize_like_bash(segment);
        let Some(base) = first_segment_subcommand(&parts) else {
            continue;
        };
        if subcommand_requires_boundary_review(base) {
            return Some(base.to_string());
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
enum ShellLoopKind {
    WhileRead,
    ForIn,
}

impl std::fmt::Display for ShellLoopKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WhileRead => write!(f, "while read"),
            Self::ForIn => write!(f, "for"),
        }
    }
}

fn xargs_subcommand_requires_boundary_review(parts: &[String]) -> Option<String> {
    let mut idx = 1usize;
    while idx < parts.len() {
        let token = parts[idx].as_str();
        if token == "--" {
            idx += 1;
            break;
        }
        if !token.starts_with('-') || token == "-" {
            break;
        }
        if xargs_flag_requires_value(token) {
            idx = (idx + 2).min(parts.len());
            continue;
        }
        idx += 1;
    }

    let subcommand = parts.get(idx)?;
    let base = subcommand.rsplit('/').next().unwrap_or(subcommand.as_str());
    subcommand_requires_boundary_review(base).then(|| base.to_string())
}

fn xargs_flag_requires_value(flag: &str) -> bool {
    matches!(
        flag,
        "-a" | "--arg-file"
            | "-d"
            | "--delimiter"
            | "-E"
            | "--eof"
            | "-I"
            | "--replace"
            | "-L"
            | "--max-lines"
            | "-n"
            | "--max-args"
            | "-P"
            | "--max-procs"
            | "-s"
            | "--max-chars"
            | "--process-slot-var"
    ) || flag.starts_with("--arg-file=")
        || flag.starts_with("--delimiter=")
        || flag.starts_with("--eof=")
        || flag.starts_with("--replace=")
        || flag.starts_with("--max-lines=")
        || flag.starts_with("--max-args=")
        || flag.starts_with("--max-procs=")
        || flag.starts_with("--max-chars=")
        || flag.starts_with("--process-slot-var=")
        || matches!(
            flag.as_bytes().get(1).copied(),
            Some(b'a' | b'd' | b'E' | b'I' | b'L' | b'n' | b'P' | b's')
        ) && flag.len() > 2
            && !flag.starts_with("--")
}

fn find_exec_subcommand_requires_boundary_review(parts: &[String]) -> Option<(String, String)> {
    let mut idx = 1usize;
    while idx < parts.len() {
        let token = parts[idx].as_str();
        if matches!(token, "-exec" | "-execdir" | "-ok" | "-okdir") {
            let subcommand = parts.get(idx + 1)?;
            let base = subcommand.rsplit('/').next().unwrap_or(subcommand.as_str());
            if subcommand_requires_boundary_review(base) {
                return Some((token.to_string(), base.to_string()));
            }

            idx += 2;
            while idx < parts.len() {
                let inner = parts[idx].as_str();
                if inner == ";" || inner == "+" {
                    break;
                }
                idx += 1;
            }
        }
        idx += 1;
    }
    None
}

fn fd_subcommand_requires_boundary_review(parts: &[String]) -> Option<(String, String)> {
    let mut idx = 1usize;
    while idx < parts.len() {
        let token = parts[idx].as_str();
        if let Some(subcommand) = token.strip_prefix("--exec=") {
            let base = subcommand.rsplit('/').next().unwrap_or(subcommand);
            if subcommand_requires_boundary_review(base) {
                return Some(("--exec".to_string(), base.to_string()));
            }
            idx += 1;
            continue;
        }
        if let Some(subcommand) = token.strip_prefix("--exec-batch=") {
            let base = subcommand.rsplit('/').next().unwrap_or(subcommand);
            if subcommand_requires_boundary_review(base) {
                return Some(("--exec-batch".to_string(), base.to_string()));
            }
            idx += 1;
            continue;
        }
        if matches!(token, "-x" | "--exec" | "-X" | "--exec-batch") {
            let subcommand = parts.get(idx + 1)?;
            let base = subcommand.rsplit('/').next().unwrap_or(subcommand.as_str());
            if subcommand_requires_boundary_review(base) {
                return Some((token.to_string(), base.to_string()));
            }
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

fn parallel_subcommand_requires_boundary_review(parts: &[String]) -> Option<String> {
    let separator_idx = parts
        .iter()
        .position(|part| matches!(part.as_str(), ":::" | "::::" | ":::+" | "::::+"))?;

    let mut idx = 1usize;
    while idx < separator_idx {
        let token = parts[idx].as_str();
        if token == "--" {
            idx += 1;
            continue;
        }
        if token.starts_with('-') && token != "-" {
            idx += parallel_flag_consumed_tokens(token);
            continue;
        }
        let base = token.rsplit('/').next().unwrap_or(token);
        return subcommand_requires_boundary_review(base).then(|| base.to_string());
    }
    None
}

fn parallel_flag_consumed_tokens(flag: &str) -> usize {
    match flag {
        "-a" | "--arg-file" | "-I" | "--replace" | "-j" | "--jobs" | "-n" | "--max-args" | "-N"
        | "--max-replace-args" | "-P" | "--max-procs" | "-S" | "--sshlogin" | "--tagstring"
        | "-L" => 2,
        _ if flag.starts_with("--arg-file=")
            || flag.starts_with("--replace=")
            || flag.starts_with("--jobs=")
            || flag.starts_with("--max-args=")
            || flag.starts_with("--max-replace-args=")
            || flag.starts_with("--max-procs=")
            || flag.starts_with("--sshlogin=")
            || flag.starts_with("--tagstring=") =>
        {
            1
        }
        _ => 1,
    }
}

fn check_awk_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    parts: &[String],
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let mut idx = 1usize;
    let mut skipped_program = false;

    while idx < parts.len() {
        let arg = parts[idx].as_str();
        if arg == "--" {
            idx += 1;
            break;
        }

        if let Some(path) = awk_inline_path_flag_value(arg) {
            if let Some(msg) = validate_command_path_arg(policy, path, oldpwd) {
                return Some(msg);
            }
            idx += 1;
            continue;
        }
        if awk_inline_non_path_flag_value(arg) {
            idx += 1;
            continue;
        }
        if let Some(kind) = awk_flag_value_kind(arg) {
            if let Some(value) = parts.get(idx + 1) {
                if matches!(kind, ShellFlagValueKind::Path)
                    && let Some(msg) = validate_command_path_arg(policy, value, oldpwd)
                {
                    return Some(msg);
                }
                idx += 2;
                continue;
            }
            idx += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            idx += 1;
            continue;
        }
        if !skipped_program {
            skipped_program = true;
            idx += 1;
            continue;
        }
        if let Some(msg) = validate_command_path_arg(policy, arg, oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    while idx < parts.len() {
        if let Some(msg) = validate_command_path_arg(policy, &parts[idx], oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    None
}

fn awk_flag_value_kind(flag: &str) -> Option<ShellFlagValueKind> {
    match flag {
        "-f" | "--file" | "-i" | "--include" => Some(ShellFlagValueKind::Path),
        "-F" | "--field-separator" | "-v" | "--assign" => Some(ShellFlagValueKind::Other),
        _ => None,
    }
}

fn awk_inline_path_flag_value(flag: &str) -> Option<&str> {
    flag.strip_prefix("-f")
        .filter(|value| !value.is_empty())
        .or_else(|| flag.strip_prefix("-i").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("--file="))
        .or_else(|| flag.strip_prefix("--include="))
}

fn awk_inline_non_path_flag_value(flag: &str) -> bool {
    (flag.starts_with("-F") || flag.starts_with("-v")) && flag.len() > 2 && !flag.starts_with("--")
        || flag.starts_with("--field-separator=")
        || flag.starts_with("--assign=")
}

fn check_sed_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    parts: &[String],
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let mut idx = 1usize;
    let mut skipped_program = false;

    while idx < parts.len() {
        let arg = parts[idx].as_str();
        if arg == "--" {
            idx += 1;
            break;
        }

        if let Some(path) = sed_inline_path_flag_value(arg) {
            if let Some(msg) = validate_command_path_arg(policy, path, oldpwd) {
                return Some(msg);
            }
            idx += 1;
            continue;
        }
        if sed_inline_non_path_flag_value(arg) {
            idx += 1;
            continue;
        }
        if let Some(kind) = sed_flag_value_kind(arg) {
            if let Some(value) = parts.get(idx + 1) {
                if matches!(kind, ShellFlagValueKind::Path)
                    && let Some(msg) = validate_command_path_arg(policy, value, oldpwd)
                {
                    return Some(msg);
                }
                idx += 2;
                continue;
            }
            idx += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            idx += 1;
            continue;
        }
        if !skipped_program {
            skipped_program = true;
            idx += 1;
            continue;
        }
        if let Some(msg) = validate_command_path_arg(policy, arg, oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    while idx < parts.len() {
        if let Some(msg) = validate_command_path_arg(policy, &parts[idx], oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    None
}

fn sed_flag_value_kind(flag: &str) -> Option<ShellFlagValueKind> {
    match flag {
        "-f" | "--file" => Some(ShellFlagValueKind::Path),
        "-e" | "--expression" => Some(ShellFlagValueKind::Other),
        _ => None,
    }
}

fn sed_inline_path_flag_value(flag: &str) -> Option<&str> {
    flag.strip_prefix("-f")
        .filter(|value| !value.is_empty())
        .or_else(|| flag.strip_prefix("--file="))
}

fn sed_inline_non_path_flag_value(flag: &str) -> bool {
    (flag.starts_with("-e") && flag.len() > 2 && !flag.starts_with("--"))
        || flag.starts_with("--expression=")
}

fn validate_command_path_operand(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    arg: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    (arg != "-")
        .then(|| validate_command_path_arg(policy, arg, oldpwd))
        .flatten()
}

fn check_tar_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    parts: &[String],
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let mut idx = 1usize;

    if let Some(cluster) = parts.get(1)
        && !cluster.starts_with('-')
        && cluster.chars().all(|ch| ch.is_ascii_alphabetic())
    {
        idx = 2;
        for flag in cluster.chars() {
            if tar_old_style_flag_has_path_value(flag) {
                if let Some(value) = parts.get(idx)
                    && let Some(msg) = validate_command_path_operand(policy, value, oldpwd)
                {
                    return Some(msg);
                }
                idx += 1;
            }
        }
    } else {
        while idx < parts.len() {
            let arg = parts[idx].as_str();
            if arg == "--" {
                idx += 1;
                break;
            }

            if let Some(path) = tar_inline_path_flag_value(arg) {
                if let Some(msg) = validate_command_path_operand(policy, path, oldpwd) {
                    return Some(msg);
                }
                idx += 1;
                continue;
            }
            if let Some(kind) = tar_flag_value_kind(arg) {
                if let Some(value) = parts.get(idx + 1) {
                    if matches!(kind, ShellFlagValueKind::Path)
                        && let Some(msg) = validate_command_path_operand(policy, value, oldpwd)
                    {
                        return Some(msg);
                    }
                    idx += 2;
                    continue;
                }
                idx += 1;
                continue;
            }
            if arg.starts_with('-') && arg != "-" {
                idx += 1;
                continue;
            }
            break;
        }
    }

    while idx < parts.len() {
        if let Some(msg) = validate_command_path_operand(policy, &parts[idx], oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    None
}

fn tar_old_style_flag_has_path_value(flag: char) -> bool {
    matches!(flag, 'f' | 'C' | 'T' | 'X' | 'g')
}

fn tar_flag_value_kind(flag: &str) -> Option<ShellFlagValueKind> {
    match flag {
        "-f"
        | "--file"
        | "-C"
        | "--directory"
        | "-T"
        | "--files-from"
        | "-X"
        | "--exclude-from"
        | "-g"
        | "--listed-incremental" => Some(ShellFlagValueKind::Path),
        _ => None,
    }
}

fn tar_inline_path_flag_value(flag: &str) -> Option<&str> {
    flag.strip_prefix("-f")
        .filter(|value| !value.is_empty())
        .or_else(|| flag.strip_prefix("-C").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("-T").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("-X").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("-g").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("--file="))
        .or_else(|| flag.strip_prefix("--directory="))
        .or_else(|| flag.strip_prefix("--files-from="))
        .or_else(|| flag.strip_prefix("--exclude-from="))
        .or_else(|| flag.strip_prefix("--listed-incremental="))
}

fn check_patch_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    parts: &[String],
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    let mut idx = 1usize;
    while idx < parts.len() {
        let arg = parts[idx].as_str();
        if arg == "--" {
            idx += 1;
            break;
        }

        if let Some(path) = patch_inline_path_flag_value(arg) {
            if let Some(msg) = validate_command_path_operand(policy, path, oldpwd) {
                return Some(msg);
            }
            idx += 1;
            continue;
        }
        if let Some(kind) = patch_flag_value_kind(arg) {
            if let Some(value) = parts.get(idx + 1) {
                if matches!(kind, ShellFlagValueKind::Path)
                    && let Some(msg) = validate_command_path_operand(policy, value, oldpwd)
                {
                    return Some(msg);
                }
                idx += 2;
                continue;
            }
            idx += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            idx += 1;
            continue;
        }
        if let Some(msg) = validate_command_path_operand(policy, arg, oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    while idx < parts.len() {
        if let Some(msg) = validate_command_path_operand(policy, &parts[idx], oldpwd) {
            return Some(msg);
        }
        idx += 1;
    }

    None
}

fn patch_flag_value_kind(flag: &str) -> Option<ShellFlagValueKind> {
    match flag {
        "-i" | "--input" | "-o" | "--output" | "-d" | "--directory" | "-r" | "--reject-file" => {
            Some(ShellFlagValueKind::Path)
        }
        _ => None,
    }
}

fn patch_inline_path_flag_value(flag: &str) -> Option<&str> {
    flag.strip_prefix("-i")
        .filter(|value| !value.is_empty())
        .or_else(|| flag.strip_prefix("-o").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("-d").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("-r").filter(|value| !value.is_empty()))
        .or_else(|| flag.strip_prefix("--input="))
        .or_else(|| flag.strip_prefix("--output="))
        .or_else(|| flag.strip_prefix("--directory="))
        .or_else(|| flag.strip_prefix("--reject-file="))
}

/// Check a single (non-compound) command for path boundary violations.
fn check_single_command_path_boundary(
    policy: &astra_runtime::tool_sandbox::SandboxPolicy,
    command: &str,
    oldpwd: Option<&std::path::Path>,
) -> Option<String> {
    for inner in process_substitution_commands(command) {
        if let Some(msg) = check_bash_path_boundary_with_oldpwd(policy, inner, oldpwd) {
            return Some(msg);
        }
    }
    if let Some(msg) = check_redirection_path_boundary(policy, command, oldpwd) {
        return Some(msg);
    }

    let parts = shell_tokenize_like_bash(command);
    if parts.is_empty() {
        return None;
    }

    let base = parts[0].rsplit('/').next().unwrap_or(parts[0].as_str());

    if is_shell_interpreter_command(base) {
        return check_shell_interpreter_path_boundary(policy, &parts, oldpwd);
    }
    if base == "awk" {
        return check_awk_path_boundary(policy, &parts, oldpwd);
    }
    if base == "sed" {
        return check_sed_path_boundary(policy, &parts, oldpwd);
    }
    if base == "tar" {
        return check_tar_path_boundary(policy, &parts, oldpwd);
    }
    if base == "patch" {
        return check_patch_path_boundary(policy, &parts, oldpwd);
    }

    if base == "xargs" {
        if let Some(subcommand) = xargs_subcommand_requires_boundary_review(&parts) {
            return Some(format!(
                "{}The command uses `xargs {subcommand}` so file paths may be supplied from stdin and cannot be statically validated against the project directory '{}'. Ask the user for permission before using xargs to fan out file-access or shell commands.",
                super::SANDBOX_DENIED_PREFIX,
                policy.project_root.display(),
            ));
        }
        return None;
    }
    if base == "find" {
        if let Some((action, subcommand)) = find_exec_subcommand_requires_boundary_review(&parts) {
            return Some(format!(
                "{}The command uses `find {action} {subcommand}` so file paths may be supplied from find matches and cannot be statically validated against the project directory '{}'. Ask the user for permission before using find fan-out with file-access or shell commands.",
                super::SANDBOX_DENIED_PREFIX,
                policy.project_root.display(),
            ));
        }
        return None;
    }
    if matches!(base, "fd" | "fdfind") {
        if let Some((action, subcommand)) = fd_subcommand_requires_boundary_review(&parts) {
            return Some(format!(
                "{}The command uses `fd {action} {subcommand}` so file paths may be supplied from search matches and cannot be statically validated against the project directory '{}'. Ask the user for permission before using fd fan-out with file-access or shell commands.",
                super::SANDBOX_DENIED_PREFIX,
                policy.project_root.display(),
            ));
        }
        return None;
    }
    if base == "parallel" {
        if let Some(subcommand) = parallel_subcommand_requires_boundary_review(&parts) {
            return Some(format!(
                "{}The command uses `parallel {subcommand}` so file paths may be supplied from batch inputs and cannot be statically validated against the project directory '{}'. Ask the user for permission before using parallel fan-out with file-access or shell commands.",
                super::SANDBOX_DENIED_PREFIX,
                policy.project_root.display(),
            ));
        }
        return None;
    }

    if !is_boundary_sensitive_file_access_command(base) {
        return None;
    }

    // Check each non-flag argument for path boundary violations.
    for arg in &parts[1..] {
        // Skip flags
        if arg.starts_with('-') {
            continue;
        }
        if let Some(msg) = validate_command_path_arg(policy, arg, oldpwd) {
            return Some(msg);
        }
    }
    None
}

/// Check if a command is potentially destructive and return a warning.
fn destructive_command_warning(command: &str) -> Option<&'static str> {
    // Static patterns checked in order; first match wins.
    static PATTERNS: &[(&str, &str)] = &[
        // Git — data loss
        (
            "git reset --hard",
            "⚠️ Warning: may discard uncommitted changes",
        ),
        (
            "git push --force",
            "⚠️ Warning: may overwrite remote history",
        ),
        ("git push -f", "⚠️ Warning: may overwrite remote history"),
        (
            "git clean -f",
            "⚠️ Warning: may permanently delete untracked files",
        ),
        (
            "git checkout -- .",
            "⚠️ Warning: may discard all working tree changes",
        ),
        (
            "git restore -- .",
            "⚠️ Warning: may discard all working tree changes",
        ),
        (
            "git stash drop",
            "⚠️ Warning: may permanently remove stashed changes",
        ),
        (
            "git stash clear",
            "⚠️ Warning: may permanently remove all stashed changes",
        ),
        ("git branch -D", "⚠️ Warning: may force-delete a branch"),
        // Git — safety bypass
        ("--no-verify", "⚠️ Warning: skipping safety hooks"),
        // File deletion
        (
            "rm -rf /",
            "⚠️ Warning: recursive force-remove from root — extremely dangerous",
        ),
        // Database
        ("DROP TABLE", "⚠️ Warning: may drop database table"),
        ("DROP DATABASE", "⚠️ Warning: may drop entire database"),
        ("TRUNCATE TABLE", "⚠️ Warning: may truncate database table"),
        // Infrastructure
        (
            "terraform destroy",
            "⚠️ Warning: may destroy infrastructure",
        ),
        (
            "kubectl delete",
            "⚠️ Warning: may delete Kubernetes resources",
        ),
    ];
    for &(pattern, warning) in PATTERNS {
        if command.contains(pattern) {
            return Some(warning);
        }
    }
    None
}

fn shell_segment_base_command(segment: &str) -> Option<String> {
    let mut tokens = segment.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        let base = token.rsplit('/').next().unwrap_or(token);
        let lower = base.to_ascii_lowercase();
        match lower.as_str() {
            "sudo" | "command" | "nohup" | "setsid" | "nice" | "time" | "strace" | "ltrace"
            | "taskset" | "exec" | "builtin" => continue,
            "env" => {
                while tokens.peek().is_some_and(|next| next.contains('=')) {
                    tokens.next();
                }
                continue;
            }
            _ => return Some(lower),
        }
    }
    None
}

fn forbidden_name_based_process_kill(command: &str) -> Option<&'static str> {
    for segment in bash_command_segments(command) {
        let Some(base) = shell_segment_base_command(segment) else {
            continue;
        };
        if matches!(base.as_str(), "pkill" | "killall") {
            return Some(
                "Error: name-based process killing commands (`pkill` / `killall`) are not allowed in this shared environment. Find the specific PID first and then use `kill <PID>`.",
            );
        }
    }
    None
}

fn destructive_powershell_warning(command: &str) -> Option<&'static str> {
    static PATTERNS: &[(&str, &str)] = &[
        (
            "remove-item -recurse",
            "⚠️ Warning: may recursively delete files or directories",
        ),
        (
            "remove-item -force",
            "⚠️ Warning: may forcibly delete files or directories",
        ),
        (
            "stop-process -force",
            "⚠️ Warning: may forcibly terminate processes",
        ),
        (
            "set-executionpolicy",
            "⚠️ Warning: may change PowerShell execution policy",
        ),
    ];
    let lower = command.to_ascii_lowercase();
    for &(pattern, warning) in PATTERNS {
        if lower.contains(pattern) {
            return Some(warning);
        }
    }
    None
}

/// Execute a command with process group isolation and timeout.
///
/// This ensures child processes are properly cleaned up even if:
/// - The parent process receives SIGINT (Ctrl+C)
/// - The command times out
/// - The tokio runtime shuts down mid-execution
///
/// Returns the Output on success, or an error message on failure/timeout.
fn run_command_with_cleanup(
    cmd: &mut Command,
    timeout_secs: f64,
) -> Result<std::process::Output, String> {
    // Create a new process group so we can kill the entire tree on timeout/signal.
    // This prevents orphaned child processes from becoming zombies.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Error: {e}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    // Kill entire process group (command + all children)
                    sigkill_process_group(&mut child);
                    // Reap the zombie process to prevent resource leak
                    let _ = child.wait();
                    return Err(format!("Error: command timed out after {timeout_secs}s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Error: {e}")),
        }
    }

    child.wait_with_output().map_err(|e| format!("Error: {e}"))
}

// ---------------------------------------------------------------------------
// Streaming output support
// ---------------------------------------------------------------------------

/// Maximum output size before truncation (30K chars).
const MAX_OUTPUT_CHARS: usize = 30_000;

/// Size watchdog poll interval for backgrounded tasks.
const SIZE_WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

/// Production default for bash's post-exit pipe read timeout. After bash
/// exits, we wait this long to drain stdout/stderr before giving up on
/// orphaned background descendants keeping the pipes open.
const BASH_PIPE_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// SIGKILL the child's entire process group via `killpg(2)` (the child must
/// have been spawned with `process_group(0)`), then SIGKILL the child
/// directly as a belt-and-suspenders fallback. Uses a direct syscall
/// instead of fork-execing `/usr/bin/kill` — cheaper and doesn't block
/// a surrounding async runtime for the fork+exec latency.
///
/// Failures are silently ignored because the child may already have been
/// reaped by the caller in a race; this helper is strictly best-effort.
fn sigkill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // `child.id()` returns the OS PID of a not-yet-reaped child;
        // `process_group(0)` at spawn guarantees pgid == pid. `killpg` is
        // signal-safe and ignores ESRCH if the group has already exited.
        // Skip the `killpg` call if the PID exceeds `i32::MAX` (would wrap
        // to a negative value and target the wrong group); on real Linux
        // `pid_max` is far below that ceiling so this is a theoretical
        // guard, not a hot path.
        if let Ok(raw) = i32::try_from(child.id()) {
            let pgid = nix::unistd::Pid::from_raw(raw);
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Resolve the pipe-read timeout. Tests can shorten it via
/// `set_test_bash_pipe_read_timeout` to avoid waiting the real 500ms.
fn bash_pipe_read_timeout() -> Duration {
    #[cfg(test)]
    if let Some(ms) = TEST_BASH_PIPE_READ_TIMEOUT_MS.with(|c| *c.borrow()) {
        return Duration::from_millis(ms);
    }
    BASH_PIPE_READ_TIMEOUT
}

#[cfg(test)]
thread_local! {
    static TEST_BASH_PIPE_READ_TIMEOUT_MS: std::cell::RefCell<Option<u64>> =
        const { std::cell::RefCell::new(None) };
}

/// Execute a command with streaming output and optional auto-backgrounding on timeout.
///
/// - Streams stdout/stderr incrementally via `on_output` callback
/// - On timeout: backgrounds the process instead of killing it (if `allow_background` is true)
/// - Watchdog kills backgrounded processes after 30 minutes
///
/// Returns (output_text, exit_code, was_backgrounded).
fn run_command_streaming(
    cmd: &mut Command,
    timeout_secs: f64,
    allow_background: bool,
    on_output: Option<&dyn Fn(&str)>,
) -> Result<StreamingResult, String> {
    use std::io::Read;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Error: {e}"))?;

    let mut stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let mut stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

    // Read output in a separate thread to avoid blocking.
    let (tx, rx) = std::sync::mpsc::channel::<OutputChunk>();
    let tx2 = tx.clone();

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = tx.send(OutputChunk::Stdout(s));
                }
                Err(_) => break,
            }
        }
    });

    let stderr_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = tx2.send(OutputChunk::Stderr(s));
                }
                Err(_) => break,
            }
        }
    });

    let mut output = String::new();
    let mut truncated = false;
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);

    loop {
        // Drain available output chunks.
        while let Ok(chunk) = rx.try_recv() {
            let text = match &chunk {
                OutputChunk::Stdout(s) | OutputChunk::Stderr(s) => s.as_str(),
            };
            if !truncated {
                if output.len() + text.len() > MAX_OUTPUT_CHARS {
                    let remaining = MAX_OUTPUT_CHARS.saturating_sub(output.len());
                    // Find a valid UTF-8 boundary for truncation.
                    let safe_end = text
                        .char_indices()
                        .take_while(|(i, _)| *i <= remaining)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    output.push_str(&text[..safe_end]);
                    output.push_str("\n... [output truncated] ...");
                    truncated = true;
                } else {
                    output.push_str(text);
                }
            }
            if let Some(cb) = on_output {
                cb(text);
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited — drain remaining output.
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                while let Ok(chunk) = rx.try_recv() {
                    let text = match &chunk {
                        OutputChunk::Stdout(s) | OutputChunk::Stderr(s) => s.as_str(),
                    };
                    if !truncated && output.len() + text.len() <= MAX_OUTPUT_CHARS {
                        output.push_str(text);
                    }
                    if let Some(cb) = on_output {
                        cb(text);
                    }
                }
                return Ok(StreamingResult {
                    output,
                    exit_code: status.code().unwrap_or(-1),
                    backgrounded: false,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    if allow_background {
                        // Auto-background: detach and start size watchdog.
                        let pid = child.id();
                        output.push_str(&format!(
                            "\n[Command timed out after {timeout_secs}s — backgrounded as PID {pid}]"
                        ));
                        // Spawn watchdog thread to kill if output grows too large.
                        std::thread::spawn(move || {
                            size_watchdog(child, stdout_thread, stderr_thread);
                        });
                        return Ok(StreamingResult {
                            output,
                            exit_code: -1,
                            backgrounded: true,
                        });
                    } else {
                        // Hard kill.
                        sigkill_process_group(&mut child);
                        let _ = child.wait();
                        let _ = stdout_thread.join();
                        let _ = stderr_thread.join();
                        output
                            .push_str(&format!("\nError: command timed out after {timeout_secs}s"));
                        return Ok(StreamingResult {
                            output,
                            exit_code: 143, // SIGTERM
                            backgrounded: false,
                        });
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Error: {e}")),
        }
    }
}

/// Result from streaming command execution.
#[allow(dead_code)]
struct StreamingResult {
    output: String,
    exit_code: i32,
    backgrounded: bool,
}

enum OutputChunk {
    Stdout(String),
    Stderr(String),
}

/// Time-limit watchdog for backgrounded processes.
/// Kills the process after 30 minutes to prevent indefinite resource consumption.
fn size_watchdog(
    mut child: std::process::Child,
    stdout_thread: std::thread::JoinHandle<()>,
    stderr_thread: std::thread::JoinHandle<()>,
) {
    let start = std::time::Instant::now();
    // Give backgrounded process up to 30 minutes.
    let max_duration = Duration::from_secs(30 * 60);

    loop {
        std::thread::sleep(SIZE_WATCHDOG_INTERVAL);

        // Check if process has exited.
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        // Kill if running too long.
        if start.elapsed() > max_duration {
            sigkill_process_group(&mut child);
            let _ = child.wait();
            break;
        }
    }

    // Clean up threads.
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
}

/// Default cap on grep result lines when no explicit limit is given.
/// Prevents unbounded output from broad patterns on large repos.
/// The LLM can pass `head_limit=0` to override.
const GREP_DEFAULT_HEAD_LIMIT: usize = 100;

/// Run a read-only command (grep/glob) with timeout, capturing only stdout.
/// Unlike `run_command_streaming`, stderr is captured separately and not mixed
/// into the output — the caller gets clean stdout content plus stderr for errors.
/// Returns `(stdout, stderr, exit_code, timed_out)`.
fn run_readonly_command_with_partial(
    cmd: &mut Command,
    timeout_secs: f64,
) -> Result<(String, String, i32, bool), String> {
    use std::io::Read;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Error: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let mut stderr_pipe = child.stderr.take().ok_or("Failed to capture stderr")?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                }
                Err(_) => break,
            }
        }
    });

    // Capture stderr in a separate thread (for error reporting only, not mixed into output)
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let mut output = String::new();
    let max_bytes = MAX_OUTPUT_CHARS;
    let mut capped = false;
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);

    loop {
        while let Ok(chunk) = rx.try_recv() {
            if !capped {
                if output.len() + chunk.len() > max_bytes {
                    let remaining = max_bytes.saturating_sub(output.len());
                    let safe = chunk.floor_char_boundary(remaining);
                    output.push_str(&chunk[..safe]);
                    capped = true;
                } else {
                    output.push_str(&chunk);
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = reader.join();
                let stderr_text = stderr_reader.join().unwrap_or_default();
                while let Ok(chunk) = rx.try_recv() {
                    if !capped && output.len() + chunk.len() <= max_bytes {
                        output.push_str(&chunk);
                    }
                }
                return Ok((output, stderr_text, status.code().unwrap_or(-1), false));
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    // Kill the entire process group (catches child processes).
                    // Fall back to direct kill so child.wait() never blocks forever.
                    sigkill_process_group(&mut child);
                    let _ = child.wait();
                    let _ = reader.join();
                    let _ = stderr_reader.join();
                    // Drain any remaining buffered output
                    while let Ok(chunk) = rx.try_recv() {
                        if !capped && output.len() + chunk.len() <= max_bytes {
                            output.push_str(&chunk);
                        }
                    }
                    // Drop the last line — it may be incomplete
                    if let Some(last_nl) = output.rfind('\n') {
                        output.truncate(last_nl);
                    }
                    return Ok((output, String::new(), -1, true));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Error: {e}")),
        }
    }
}

const DEFAULT_SEARCH_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    "target",
    "dist",
    "build",
    "coverage",
    "htmlcov",
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".cache",
    "out",
];

fn append_default_grep_excludes(cmd: &mut Command) {
    cmd.arg("--binary-files=without-match");
    cmd.arg("--devices=skip");
    for dir in DEFAULT_SEARCH_EXCLUDE_DIRS {
        cmd.arg("--exclude-dir").arg(dir);
    }
}

fn default_find_prune_clause() -> String {
    let joined = DEFAULT_SEARCH_EXCLUDE_DIRS
        .iter()
        .map(|dir| format!("-name {}", shell_escape(dir)))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!("\\( -type d \\( {joined} \\) -prune \\)")
}

/// SSRF protection: check if a URL targets internal/private networks.
/// Returns Some(reason) if blocked, None if safe.
fn is_ssrf_target(url: &str) -> Option<&'static str> {
    // Extract host from URL (simple parsing, handles http://host:port/path)
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = after_scheme.split('/').next()?;
    // Handle userinfo@ prefix
    let host_port = authority.split('@').next_back()?;
    // Handle IPv6 brackets: [::1]:port → extract [::1]
    let host = if host_port.starts_with('[') {
        // IPv6: take everything up to and including the closing bracket
        host_port
            .split(']')
            .next()
            .map(|s| format!("{s}]"))
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    let lower = host.to_ascii_lowercase();

    // Block localhost variants
    if lower == "localhost"
        || lower == "127.0.0.1"
        || lower == "0.0.0.0"
        || lower == "::1"
        || lower == "[::1]"
        || lower.ends_with(".localhost")
    {
        return Some("localhost access blocked");
    }
    // Block AWS/cloud metadata endpoints
    if lower == "169.254.169.254" || lower == "metadata.google.internal" {
        return Some("cloud metadata endpoint blocked");
    }
    // Block private IP ranges (RFC 1918 + link-local)
    if lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.") && is_private_172(&lower)
        || lower.starts_with("169.254.")
        || lower.starts_with("fc")
        || lower.starts_with("fd")
    {
        return Some("private network access blocked");
    }
    None
}

/// Check if a 172.x.x.x address is in the private range 172.16-31.x.x
fn is_private_172(host: &str) -> bool {
    if let Some(second) = host.strip_prefix("172.").and_then(|r| r.split('.').next())
        && let Ok(n) = second.parse::<u8>()
    {
        return (16..=31).contains(&n);
    }
    false
}

impl ToolExecutor {
    fn run_shell_output_with_program(
        &self,
        program: &str,
        shell_flag: &str,
        command: &str,
        timeout_secs: f64,
        harden_command: bool,
    ) -> Result<std::process::Output, String> {
        let effective_command = if harden_command {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard {
                if !matches!(policy.mode, SandboxMode::Permissive) {
                    wrap_command_with_limits(policy, command)
                } else {
                    command.to_string()
                }
            } else {
                command.to_string()
            }
        } else {
            command.to_string()
        };

        let mut child_cmd = Command::new(program);
        child_cmd
            .arg(shell_flag)
            .arg(&effective_command)
            .current_dir(self.effective_project_root())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Pass env overlay vars to child process so env_set values are visible.
        super::apply_env_overlay(&mut child_cmd);

        // Create a new process group so we can kill the entire tree on timeout.
        // This prevents orphaned git/curl/etc. child processes.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            child_cmd.process_group(0); // child becomes its own process group leader
        }

        // Apply sandbox environment filtering
        {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard
                && !matches!(policy.mode, SandboxMode::Permissive)
                && let Err(e) = sandbox_command(policy, &mut child_cmd)
            {
                eprintln!("[sandbox] failed to apply policy: {e}");
                return Err(format!("Error: sandbox policy application failed: {e}"));
            }
        }

        let mut child = child_cmd.spawn().map_err(|e| format!("Error: {e}"))?;

        // Take ownership of stdout/stderr handles before the wait loop.
        // This allows us to read available output even if background processes keep pipes open.
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
        let exit_status;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_status = status;
                    break;
                }
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        // Kill entire process group (bash + all children)
                        sigkill_process_group(&mut child);
                        // Reap the zombie process to prevent resource leak
                        let _ = child.wait();
                        return Err(format!("Error: command timed out after {timeout_secs}s"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("Error: {e}")),
            }
        }

        // Read available output with a short timeout. Don't use wait_with_output()
        // because it blocks until ALL pipe handles are closed. When the command
        // spawns background processes (e.g., `python app.py &`), those processes
        // inherit the pipes and keep them open indefinitely, causing a hang.
        //
        // Solution: Set pipes to non-blocking mode and read until timeout.
        use std::io::Read;
        let read_timeout = bash_pipe_read_timeout();

        // Helper to read from a pipe with timeout using non-blocking I/O
        fn read_with_timeout(mut pipe: std::process::ChildStdout, timeout: Duration) -> Vec<u8> {
            #[cfg(unix)]
            {
                use std::os::unix::io::{AsRawFd, BorrowedFd};
                // Set non-blocking mode
                let fd = pipe.as_raw_fd();
                // SAFETY: pipe is valid and we're not closing it
                let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
                if let Ok(flags) = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_GETFL) {
                    let new_flags = nix::fcntl::OFlag::from_bits_truncate(flags)
                        | nix::fcntl::OFlag::O_NONBLOCK;
                    let _ = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_SETFL(new_flags));
                }
            }

            let deadline = std::time::Instant::now() + timeout;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                if std::time::Instant::now() > deadline {
                    break;
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No data available right now, wait a bit
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            buf
        }

        fn read_stderr_with_timeout(
            mut pipe: std::process::ChildStderr,
            timeout: Duration,
        ) -> Vec<u8> {
            #[cfg(unix)]
            {
                use std::os::unix::io::{AsRawFd, BorrowedFd};
                let fd = pipe.as_raw_fd();
                let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
                if let Ok(flags) = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_GETFL) {
                    let new_flags = nix::fcntl::OFlag::from_bits_truncate(flags)
                        | nix::fcntl::OFlag::O_NONBLOCK;
                    let _ = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_SETFL(new_flags));
                }
            }

            let deadline = std::time::Instant::now() + timeout;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                if std::time::Instant::now() > deadline {
                    break;
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            buf
        }

        let stdout_buf = stdout_handle
            .map(|h| read_with_timeout(h, read_timeout))
            .unwrap_or_default();
        let stderr_buf = stderr_handle
            .map(|h| read_stderr_with_timeout(h, read_timeout))
            .unwrap_or_default();

        Ok(std::process::Output {
            status: exit_status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }

    pub(crate) fn run_shell_output(
        &self,
        command: &str,
        timeout_secs: f64,
    ) -> Result<std::process::Output, String> {
        self.run_shell_output_with_program("bash", "-c", command, timeout_secs, true)
    }

    fn run_powershell_output(
        &self,
        command: &str,
        timeout_secs: f64,
    ) -> Result<std::process::Output, String> {
        let program = find_powershell_program().ok_or_else(|| {
            "Error: PowerShell not available. Install 'pwsh' (PowerShell 7+) or 'powershell'."
                .to_string()
        })?;
        self.run_shell_output_with_program(program, "-Command", command, timeout_secs, false)
    }

    /// Register file paths that were read by a successful bash command so
    /// the read-before-edit gate accepts subsequent writes. Safe to call
    /// speculatively — unknown or non-existent paths are silently ignored
    /// by the underlying `register_external_read`.
    fn register_bash_read_targets(&self, command: &str) {
        let intent = astra_runtime::bash_intent::analyze_bash_command(command);
        // `intent.read_targets` is already filtered: `analyze_bash_command`
        // only harvests paths from non-mutating segments, so a mixed command
        // like `cat a.rs && echo >> b.rs` yields `["a.rs"]` — safe to register.
        for rel in intent.read_targets {
            let path = std::path::Path::new(&rel);
            let abs = if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.project_root.join(path)
            };
            if abs.exists() {
                self.register_external_read(&abs);
            }
        }
    }

    pub(crate) fn bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return "Error: missing 'command'".to_string(),
        };

        // Block pure sleep commands — they waste time with no useful output.
        // Only when no explicit timeout is set (explicit timeout = intentional test usage).
        // Matches: "sleep N", "sleep 3.5", but not "sleep 1 && echo done" (has useful work).
        if args.get("timeout").is_none() {
            let trimmed = command.trim();
            if trimmed.starts_with("sleep ")
                && !trimmed.contains("&&")
                && !trimmed.contains("||")
                && !trimmed.contains(';')
                && !trimmed.contains('|')
            {
                return "⚠ sleep commands are not useful — they waste time without producing output. \
                        Remove the sleep and proceed with your next action."
                    .to_string();
            }
        }

        // Nudge: redirect `git diff <range>` to the built-in git_diff/git_show tools.
        // Large multi-commit diffs via bash can timeout or produce huge uncontrolled output,
        // while built-in tools have output budgets and pressure-scaling.
        // We don't hard-block — instead, auto-pipe through `head -c` to prevent the
        // pipe buffer stall that causes timeouts on large diffs.
        let command = {
            let trimmed = command.trim();
            if (trimmed.starts_with("git diff ") || trimmed.starts_with("git log "))
                && !trimmed.contains("--stat")
                && !trimmed.contains("--name")
                && !trimmed.contains("| head")
                && !trimmed.contains("| tail")
                && (trimmed.contains("..") || trimmed.contains("HEAD~"))
            {
                // Auto-truncate to prevent pipe stall; append a hint so the agent
                // knows the output may be incomplete and can use built-in tools.
                std::borrow::Cow::Owned(format!("{trimmed} | head -c 30000"))
            } else {
                std::borrow::Cow::Borrowed(command)
            }
        };
        let command: &str = &command;

        // Use explicit timeout if provided, otherwise pick an adaptive default:
        // Tier 1 (5s):  instant commands — no I/O beyond trivial reads
        // Tier 2 (10s): fast read commands — cat, head, file stat
        // Tier 3 (15s): search/traversal — grep, find, ripgrep
        // Tier 4 (30s): everything else (build, test, network)
        let timeout_secs = args
            .get("timeout")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| {
                let cmd_base = command.split_whitespace().next().unwrap_or("");
                match cmd_base {
                    // Tier 1: instant — no real I/O
                    "echo" | "printf" | "true" | "false" | "pwd" | "whoami" | "date"
                    | "basename" | "dirname" | "which" | "env" | "hostname" | "uname" | "id"
                    | "tty" | "nproc" | "arch" | "yes" => 5.0,
                    // Tier 2: fast reads — single file or dir stat
                    "cat" | "head" | "tail" | "wc" | "stat" | "file" | "ls" | "readlink"
                    | "realpath" | "md5sum" | "sha256sum" | "du" | "df" | "touch" | "mkdir"
                    | "cp" | "mv" | "rm" | "ln" | "chmod" | "chown" => 10.0,
                    // Tier 3: search/traversal — scan many files but bounded
                    "grep" | "rg" | "find" | "fd" | "ag" | "awk" | "sed" | "sort" | "uniq"
                    | "cut" | "tr" | "diff" | "comm" | "xargs" | "tree" | "jq" | "yq"
                    | "column" | "tee" => 15.0,
                    // Tier 4: everything else (compilation, network, etc.)
                    _ => 30.0,
                }
            });

        // Sandbox path boundary check for bash commands.
        // If the sandbox is active, extract file path arguments from the command
        // and reject the command if any path escapes the project boundary.
        // This closes the loophole where read_file is blocked by the sandbox
        // but `cat /outside/path` bypasses it.
        {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard
                && !matches!(policy.mode, SandboxMode::Permissive)
            {
                if let Some(msg) = check_bash_path_boundary(policy, command) {
                    return msg;
                }
            }
        }

        if let Some(msg) = forbidden_name_based_process_kill(command) {
            return msg.to_string();
        }

        match self.run_shell_output(command, timeout_secs) {
            Err(error) => error,
            Ok(out) => {
                // Register any files read by this bash invocation so the
                // read-before-edit gate accepts subsequent writes. Without
                // this, `bash cat path` inspection creates a deadlock:
                // model reads via bash, tries to edit, gate rejects with
                // "has not been read yet", model reads again via bash, …
                if out.status.success() {
                    self.register_bash_read_targets(command);
                }

                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = String::new();

                // Prepend destructive command warning if applicable
                if let Some(warning) = destructive_command_warning(command) {
                    result.push_str(warning);
                    result.push('\n');
                }

                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
                }

                let exit_code = out.status.code().unwrap_or(-1);

                if result.is_empty() || (result.trim().is_empty() && !result.contains("⚠️")) {
                    return if out.status.success() {
                        "(no output)".to_string()
                    } else {
                        // Use command semantics to interpret exit code
                        let sem = interpret_exit_code(command, exit_code);
                        if let Some(note) = sem.note {
                            note.to_string()
                        } else if sem.is_error {
                            format!("Error: command failed (exit code {exit_code})")
                        } else {
                            format!("(exit code {exit_code})")
                        }
                    };
                }

                // Budget-pressure-aware truncation (was hardcoded 20KB)
                let limit = self.scaled_output_limit();
                if result.len() > limit {
                    // Prefer cutting at newline boundary
                    let end = result.floor_char_boundary(limit);
                    let cut = result[..end]
                        .rfind('\n')
                        .filter(|&pos| pos > end / 2)
                        .map(|pos| pos + 1)
                        .unwrap_or(end);
                    result.truncate(cut);
                    result.push_str("\n[truncated]");
                }

                // For build/test commands, provide structured output with iteration tracking
                if super::build_test::is_build_test_command(command) {
                    let mut parsed =
                        super::build_test::parse_build_test_output(&result, out.status.code());
                    if !parsed.error_locations.is_empty() {
                        parsed.enrich_with_scope(&self.project_root);
                    }
                    // Gap 2: publish failing test / error messages to the
                    // SelfModel surface so the agent perceives which tests
                    // are currently red on its next turn.
                    if parsed.tests_failed > 0 || !parsed.error_messages.is_empty() {
                        if let Some(session_lock) = &self.observability_session
                            && let Ok(mut session) = session_lock.write()
                        {
                            let names: Vec<String> = parsed
                                .error_messages
                                .iter()
                                .take(8)
                                .map(|m| {
                                    m.lines()
                                        .next()
                                        .unwrap_or(m)
                                        .trim()
                                        .chars()
                                        .take(120)
                                        .collect()
                                })
                                .filter(|s: &String| !s.is_empty())
                                .collect();
                            session.record_failing_test_names(names);
                        }
                    }
                    let delta = {
                        let mut tracker = self
                            .build_test_tracker
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if tracker.command_changed(command) {
                            tracker.reset();
                        }
                        tracker.record(&parsed, command)
                    };
                    let delta_summary = delta.to_summary();
                    if delta_summary.is_empty() {
                        return parsed.to_enhanced_output(&result);
                    }
                    return format!(
                        "{}\n\n{}",
                        delta_summary,
                        parsed.to_enhanced_output(&result)
                    );
                }

                // Append exit code context for non-zero, non-build commands
                if !out.status.success() {
                    let sem = interpret_exit_code(command, exit_code);
                    if sem.is_error {
                        result.push_str(&format!("\n(exit code {exit_code})"));
                    }
                }

                result
            }
        }
    }

    pub(crate) fn powershell(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return "Error: missing 'command'".to_string(),
        };
        let timeout_secs = args.get("timeout").and_then(Value::as_f64).unwrap_or(30.0);

        {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard
                && !matches!(policy.mode, SandboxMode::Permissive)
            {
                if let Some(msg) = check_powershell_path_boundary(policy, command) {
                    return msg;
                }
            }
        }

        match self.run_powershell_output(command, timeout_secs) {
            Err(error) => error,
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = String::new();

                if let Some(warning) = destructive_powershell_warning(command) {
                    result.push_str(warning);
                    result.push('\n');
                }

                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
                }

                let exit_code = out.status.code().unwrap_or(-1);
                if result.is_empty() || result.trim().is_empty() {
                    return if out.status.success() {
                        "(no output)".to_string()
                    } else {
                        format!("Error: command failed (exit code {exit_code})")
                    };
                }

                let limit = self.scaled_output_limit();
                if result.len() > limit {
                    let end = result.floor_char_boundary(limit);
                    let cut = result[..end]
                        .rfind('\n')
                        .filter(|&pos| pos > end / 2)
                        .map(|pos| pos + 1)
                        .unwrap_or(end);
                    result.truncate(cut);
                    result.push_str("\n[truncated]");
                }

                if !out.status.success() {
                    result.push_str(&format!("\n(exit code {exit_code})"));
                }

                result
            }
        }
    }

    pub(crate) fn grep(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let search_path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => self.project_root.clone(),
        };

        // Validate search path exists before spawning grep
        if !search_path.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                search_path.display()
            );
        }

        let include = args.get("include").and_then(Value::as_str).unwrap_or("*");
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .map(|n| n.min(10) as usize); // cap at 10 to avoid huge output
        let max_matches = args
            .get("max_matches")
            .and_then(Value::as_u64)
            .map(|n| n.max(1) as usize);
        let scope_context = args
            .get("scope_context")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let output_mode = args
            .get("output_mode")
            .and_then(Value::as_str)
            .unwrap_or("content");
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let head_limit = args
            .get("head_limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);

        let grep_flags = match output_mode {
            "files_with_matches" => "-rlHE",
            "count" => "-rcHE",
            _ => "-rnHE",
        };

        let mut cmd = Command::new("grep");
        cmd.arg(grep_flags);
        if !case_sensitive {
            cmd.arg("-i");
        }
        if let Some(ctx) = context_lines {
            cmd.arg(format!("-C{ctx}"));
        }
        if let Some(max) = max_matches {
            cmd.arg(format!("-m{max}"));
        }
        append_default_grep_excludes(&mut cmd);
        cmd.arg("--include").arg(include);
        cmd.arg(pattern).arg(&search_path);
        cmd.current_dir(&self.project_root);

        // Use streaming execution to preserve partial results on timeout
        match run_readonly_command_with_partial(&mut cmd, 30.0) {
            Ok((raw_text, stderr_text, exit_code, timed_out)) => {
                // Treat exit code 1 as "no matches" (grep convention)
                if exit_code == 1 && raw_text.trim().is_empty() {
                    let warn = stderr_text.trim();
                    return if warn.is_empty() {
                        "No matches found".to_string()
                    } else {
                        format!("No matches found (warnings: {warn})")
                    };
                }

                // If we got no output and a non-zero exit code, report error
                if raw_text.trim().is_empty() && exit_code != 0 {
                    if timed_out {
                        return "Error: grep timed out after 30s with no results. \
                             The search scope is too broad. Try: \
                             (1) search a specific subdirectory with 'path', \
                             (2) use 'include' to filter file types, \
                             (3) use a more specific pattern."
                            .to_string();
                    }
                    let detail = stderr_text.trim();
                    return if detail.is_empty() {
                        "Error: grep failed".to_string()
                    } else {
                        format!("Error: {detail}")
                    };
                }

                // For count mode, filter out zero-count lines
                let text = if output_mode == "count" {
                    raw_text
                        .lines()
                        .filter(|line| !line.ends_with(":0"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    raw_text
                };

                // Apply offset for pagination
                let lines: Vec<&str> = text.lines().collect();
                let lines = if offset > 0 {
                    if offset >= lines.len() {
                        return format!(
                            "No more results (offset {} >= {} lines)",
                            offset,
                            lines.len()
                        );
                    }
                    &lines[offset..]
                } else {
                    &lines[..]
                };

                // Apply head_limit (default GREP_DEFAULT_HEAD_LIMIT, 0 = unlimited)
                let effective_limit = match head_limit {
                    Some(0) => None,                       // explicit 0 = unlimited
                    Some(n) => Some(n),                    // explicit limit
                    None => Some(GREP_DEFAULT_HEAD_LIMIT), // default
                };
                let (lines, was_truncated_by_limit) = if let Some(limit) = effective_limit {
                    if lines.len() > limit {
                        (&lines[..limit], true)
                    } else {
                        (lines, false)
                    }
                } else {
                    (lines, false)
                };

                let mut result_text = lines.join("\n");

                // Apply per-tool output limit (centralised in per_tool_output_limit)
                let limit = self.scaled_output_limit_for("grep");
                if result_text.len() > limit {
                    result_text = result_text[..result_text.floor_char_boundary(limit)].to_string();
                    result_text.push_str("\n[truncated]");
                }

                // Append metadata about truncation/timeout
                if timed_out {
                    result_text.push_str(
                        "\n\n[grep timed out after 30s — showing partial results. \
                         Narrow the search: use 'path' for a subdirectory or 'include' for file types.]"
                    );
                }
                if was_truncated_by_limit {
                    let eff = effective_limit.unwrap_or(0);
                    result_text.push_str(&format!(
                        "\n\n[Results limited to {eff} lines. Use 'offset' to paginate or 'head_limit: 0' for unlimited.]"
                    ));
                }

                if scope_context {
                    annotate_grep_with_scope(&result_text, &self.project_root)
                } else {
                    result_text
                }
            }
            Err(e) => e,
        }
    }

    pub(crate) fn glob(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let base = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => self.project_root.clone(),
        };

        // Validate base path exists
        if !base.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                base.display()
            );
        }

        // Security: reject glob patterns with path traversal sequences
        if pattern.contains("..") || pattern.starts_with('/') || pattern.contains("~/") {
            return "Error: glob pattern must not contain '..', start with '/', or contain '~/' (path traversal risk)".to_string();
        }

        // Use fd if available (faster, respects .gitignore), fall back to find
        let shell_cmd = format!(
            "cd {} && {{ fd --type f --glob {} 2>/dev/null || find . {} -o -name {} -print | sed 's|^./||'; }} | head -100",
            shell_escape(base.to_string_lossy().as_ref()),
            shell_escape(pattern),
            default_find_prune_clause(),
            shell_escape(pattern.split('/').next_back().unwrap_or(pattern))
        );
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&shell_cmd);
        // Use 15s timeout for glob/find (directory traversal)
        match run_command_with_cleanup(&mut cmd, 15.0) {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.trim().is_empty() {
                    "No files found".to_string()
                } else {
                    // Apply per-tool output limit (centralised in per_tool_output_limit)
                    let limit = self.scaled_output_limit_for("glob");
                    let line_count = text.lines().count();
                    if text.len() > limit {
                        let end = text.floor_char_boundary(limit);
                        let cut = text[..end].rfind('\n').map(|pos| pos + 1).unwrap_or(end);
                        let shown = text[..cut].lines().count();
                        format!(
                            "{}\n[showing {shown} of {line_count} files, truncated]",
                            &text[..cut]
                        )
                    } else {
                        format!("{text}\n({line_count} files)")
                    }
                }
            }
            Err(e) => e,
        }
    }

    /// Fetch a URL and return its content (text or HTML→text).
    /// Reports HTTP status, content type, and size for transparency.
    pub(crate) fn web_fetch(&self, args: &Value) -> String {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(u) => u,
            None => return "Error: missing 'url'".to_string(),
        };
        // Basic URL validation
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return "Error: url must start with http:// or https://".to_string();
        }
        // SSRF protection: block internal/private IP ranges
        if let Some(reason) = is_ssrf_target(url) {
            return format!("Error: blocked URL ({reason})");
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or_else(|| self.scaled_output_limit().min(10_000));
        let timeout_secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(10);

        // URL cache: return cached response if fetched within TTL (15 minutes).
        // Prevents token waste when the LLM re-fetches the same documentation page.
        const URL_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        const URL_CACHE_MAX_ENTRIES: usize = 50;
        if let Ok(cache) = self.url_cache.lock()
            && let Some((cached_response, cached_at)) = cache.get(url)
            && cached_at.elapsed() < URL_CACHE_TTL
        {
            return format!(
                "[Cached response from {}s ago — content unchanged]\n{}",
                cached_at.elapsed().as_secs(),
                cached_response
            );
        }

        // Use -w to capture HTTP status code and content type for structured reporting
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-L",
            "--max-redirs",
            "5",
            "--max-time",
            &timeout_secs.to_string(),
            "--max-filesize",
            &(max_bytes * 2).to_string(),
            "-H",
            "User-Agent: astra/0.1",
            "-w",
            "\n__CURL_META__%{http_code} %{content_type} %{size_download} %{url_effective}",
            url,
        ])
        .current_dir(&self.project_root);

        // Apply sandbox environment filtering (same as bash)
        {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard
                && !matches!(policy.mode, SandboxMode::Permissive)
                && let Err(e) = sandbox_command(policy, &mut cmd)
            {
                return format!("Error: sandbox policy application failed: {e}");
            }
        }

        // Use timeout_secs + 5s buffer for our wrapper (curl has its own --max-time)
        match run_command_with_cleanup(&mut cmd, timeout_secs as f64 + 5.0) {
            Ok(out) => {
                let status = out.status;
                let raw = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !status.success() && raw.is_empty() {
                    return format!("Error: {stderr}");
                }

                // Parse metadata from -w output
                let (body, meta_line) = if let Some(idx) = raw.rfind("\n__CURL_META__") {
                    (&raw[..idx], Some(&raw[idx + "\n__CURL_META__".len()..]))
                } else {
                    (raw.as_ref(), None)
                };

                let (http_code, content_type, final_url) = if let Some(meta) = meta_line {
                    let parts: Vec<&str> = meta.splitn(4, ' ').collect();
                    (
                        parts.first().copied().unwrap_or("?"),
                        parts.get(1).copied().unwrap_or("?"),
                        parts.get(3).copied().unwrap_or(""),
                    )
                } else {
                    ("?", "?", "")
                };

                // Reject binary content types (images, audio, video, etc.)
                let ct_lower = content_type.to_lowercase();
                if ct_lower.starts_with("image/")
                    || ct_lower.starts_with("audio/")
                    || ct_lower.starts_with("video/")
                    || ct_lower.starts_with("application/octet-stream")
                    || ct_lower.starts_with("application/zip")
                    || ct_lower.starts_with("application/gzip")
                    || ct_lower.starts_with("application/pdf")
                {
                    return format!(
                        "Error: URL returned binary content ({content_type}). \
                         web_fetch only supports text content. \
                         Use bash+curl with specific flags if you need to download binary files."
                    );
                }

                // Detect cross-host redirect
                let redirected = if !final_url.is_empty() && final_url != url {
                    // Check if host changed
                    let orig_host = url.split('/').nth(2).unwrap_or("");
                    let final_host = final_url.split('/').nth(2).unwrap_or("");
                    if orig_host != final_host {
                        Some(final_url)
                    } else {
                        None
                    }
                } else {
                    None
                };

                // HTTP error codes with actionable hints
                if let Ok(code) = http_code.parse::<u16>()
                    && code >= 400
                {
                    let reason = match code {
                        401 => {
                            " (authentication required — look for an MCP tool with authenticated access or set auth headers via bash+curl)"
                        }
                        403 => {
                            " (forbidden — access permanently denied. Try a different URL or approach; do NOT retry)"
                        }
                        404 => " (page not found — verify the URL is correct)",
                        429 => {
                            " (rate limited — wait at least 30 seconds before retrying this URL)"
                        }
                        500 | 502 | 503 | 504 => {
                            " (server error — this is transient, you may retry once after a brief wait)"
                        }
                        _ => "",
                    };
                    return format!("Error: HTTP {code}{reason}\nURL: {url}");
                }

                let mut result = body.to_string();

                // Convert HTML to plain text for LLM consumption
                if looks_like_html(&result) {
                    result = html_to_text(&result);
                }

                if result.len() > max_bytes {
                    result.truncate(result.floor_char_boundary(max_bytes));
                    result.push_str("\n[truncated]");
                }

                // Append metadata footer
                let mut footer = String::new();
                if let Some(redir) = redirected {
                    footer.push_str(&format!("\n[Redirected to: {redir}]"));
                }
                if !footer.is_empty() {
                    result.push_str(&footer);
                }

                // Store successful response in cache
                if let Ok(mut cache) = self.url_cache.lock() {
                    // Evict expired entries when cache is full
                    if cache.len() >= URL_CACHE_MAX_ENTRIES {
                        cache.retain(|_, (_, ts)| ts.elapsed() < URL_CACHE_TTL);
                    }
                    // If still full after eviction, remove oldest
                    if cache.len() >= URL_CACHE_MAX_ENTRIES
                        && let Some(oldest) = cache
                            .iter()
                            .min_by_key(|(_, (_, t))| *t)
                            .map(|(k, _)| k.clone())
                    {
                        cache.remove(&oldest);
                    }
                    cache.insert(url.to_string(), (result.clone(), std::time::Instant::now()));
                }

                result
            }
            Err(e) => {
                if e.contains("timed out") {
                    format!("Error: curl timed out after {timeout_secs}s")
                } else {
                    format!("Error: curl not available — {e}")
                }
            }
        }
    }
}

/// Annotate grep results with tree-sitter scope context.
///
/// For each `file:line:content` match, looks up the containing function/class
/// using `scope_at_line()` and appends it as `  (in fn_name)` annotation.
/// Only annotates matches in files with supported languages.
/// File contents are cached to avoid re-reading the same file for multiple matches.
fn annotate_grep_with_scope(grep_output: &str, project_root: &std::path::Path) -> String {
    use super::code_intel::{detect_language, scope_at_line};
    use std::collections::HashMap;

    // Cache: file path → (source, language)
    let mut file_cache: HashMap<String, Option<(String, super::code_intel::Language)>> =
        HashMap::new();

    let mut result = String::with_capacity(grep_output.len() + grep_output.len() / 10);

    for line in grep_output.lines() {
        // Parse grep output: file:line:content or file-line-content (context)
        // Only annotate primary matches (colon separator), not context (dash separator)
        if let Some((file_part, rest)) = line.split_once(':')
            && let Some((line_num_str, _content)) = rest.split_once(':')
            && let Ok(line_num) = line_num_str.trim().parse::<usize>()
        {
            let file_path = if std::path::Path::new(file_part).is_absolute() {
                file_part.to_string()
            } else {
                project_root.join(file_part).to_string_lossy().to_string()
            };

            let cached = file_cache.entry(file_path.clone()).or_insert_with(|| {
                let path = std::path::Path::new(&file_path);
                let lang = detect_language(path)?;
                let source = std::fs::read_to_string(path).ok()?;
                Some((source, lang))
            });

            if let Some((source, lang)) = cached {
                let ctx = scope_at_line(source, *lang, line_num);
                let scope_str = if ctx.breadcrumbs.len() > 1 {
                    ctx.breadcrumbs.join(" > ")
                } else if let Some(ref sym) = ctx.symbol {
                    sym.name.clone()
                } else {
                    String::new()
                };
                if !scope_str.is_empty() {
                    result.push_str(line);
                    result.push_str("  // in ");
                    result.push_str(&scope_str);
                    result.push('\n');
                    continue;
                }
            }
        }
        result.push_str(line);
        result.push('\n');
    }

    // Remove trailing newline
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Detect HTML content by checking for common HTML markers.
fn looks_like_html(s: &str) -> bool {
    let trimmed = s.trim_start();
    trimmed.starts_with("<!DOCTYPE")
        || trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<HTML")
        // Partial HTML without doctype (common in API error pages)
        || (trimmed.starts_with('<')
            && (trimmed.contains("</head>") || trimmed.contains("</body>")))
}

/// Lightweight HTML → text conversion without external dependencies.
/// Strips tags, decodes common entities, collapses whitespace.
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();

    // 1. Remove <script> and <style> blocks (case-insensitive via manual lowering)
    for tag in &["script", "style", "noscript", "svg"] {
        loop {
            let lower = s.to_lowercase();
            let open = format!("<{}", tag);
            let close = format!("</{}>", tag);
            if let Some(start) = lower.find(&open)
                && let Some(end_rel) = lower[start..].find(&close)
            {
                let end = start + end_rel + close.len();
                s.replace_range(start..end, " ");
                continue;
            }
            break;
        }
    }

    // 2. Insert newlines for block elements
    for tag in &[
        "<br>", "<br/>", "<br />", "<BR>", "</p>", "</P>", "</div>", "</DIV>", "</li>", "</LI>",
        "</tr>", "</TR>", "</h1>", "</h2>", "</h3>", "</h4>", "</h5>", "</h6>", "</H1>", "</H2>",
        "</H3>", "</H4>", "</H5>", "</H6>",
    ] {
        s = s.replace(tag, &format!("\n{}", tag));
    }

    // 3. Strip all remaining HTML tags
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    // 4. Decode common HTML entities
    out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
        .replace("&#x2F;", "/");

    // Decode numeric character references &#NNN;
    let mut decoded = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' && chars.peek() == Some(&'#') {
            chars.next(); // consume '#'
            let mut num_str = String::new();
            while let Some(&d) = chars.peek() {
                if d == ';' {
                    chars.next();
                    break;
                }
                if d.is_ascii_digit() && num_str.len() < 7 {
                    num_str.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if let Ok(code) = num_str.parse::<u32>()
                && let Some(decoded_char) = char::from_u32(code)
            {
                decoded.push(decoded_char);
                continue;
            }
            decoded.push('&');
            decoded.push('#');
            decoded.push_str(&num_str);
        } else {
            decoded.push(ch);
        }
    }

    // 5. Collapse whitespace: runs of spaces/tabs → single space, 3+ newlines → 2
    let mut result = String::with_capacity(decoded.len());
    let mut last_was_newline = false;
    let mut consecutive_newlines = 0u32;
    let mut last_was_space = false;

    for ch in decoded.chars() {
        if ch == '\n' || ch == '\r' {
            if ch == '\r' {
                continue;
            }
            consecutive_newlines += 1;
            last_was_space = false;
            if consecutive_newlines <= 2 {
                result.push('\n');
            }
            last_was_newline = true;
        } else if ch == ' ' || ch == '\t' {
            if !last_was_space && !last_was_newline {
                result.push(' ');
            }
            last_was_space = true;
        } else {
            result.push(ch);
            last_was_newline = false;
            last_was_space = false;
            consecutive_newlines = 0;
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    fn test_executor_in(dir: &std::path::Path) -> ToolExecutor {
        ToolExecutor::new(dir)
    }

    /// Shorten the post-exit pipe read timeout for the duration of a test.
    /// Production uses 500ms; tests can drop it to e.g. 50ms so "background
    /// command does not block" assertions don't burn the budget on an
    /// artefact of the pipe-drain wait. Returns a guard that resets on drop.
    fn set_test_bash_pipe_read_timeout_ms(ms: u64) -> impl Drop {
        TEST_BASH_PIPE_READ_TIMEOUT_MS.with(|c| *c.borrow_mut() = Some(ms));
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                TEST_BASH_PIPE_READ_TIMEOUT_MS.with(|c| *c.borrow_mut() = None);
            }
        }
        Guard
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn bash_missing_command_returns_error() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn bash_echo_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo hello"}));
        assert!(result.trim().contains("hello"), "got: {result}");
    }

    /// Regression for the c49bc4a3 inspection-loop deadlock: a model that
    /// reads a file via `bash cat <path>` must be able to subsequently edit
    /// that file without the read-before-edit gate rejecting the write.
    #[test]
    fn bash_cat_registers_file_as_read_so_staleness_gate_passes() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("target.rs");
        std::fs::write(&file_path, "original\n").unwrap();

        let executor = test_executor_in(dir.path());

        // Gate rejects edits for a file that was never read.
        assert!(executor.check_staleness(&file_path).is_err());

        // Simulate the model running `cat target.rs` via bash.
        let out = executor.bash(&serde_json::json!({
            "command": format!("cat {}", file_path.display()),
        }));
        assert!(out.contains("original"), "bash output: {out}");

        // After bash-cat, the file must now be considered "read" — staleness
        // check passes, and a follow-up write is not blocked.
        assert!(
            executor.check_staleness(&file_path).is_ok(),
            "bash cat should have registered {:?} as read",
            file_path
        );
    }

    #[test]
    fn bash_mutating_command_does_not_register_paths_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("out.txt");
        let executor = test_executor_in(dir.path());

        // A redirect creates the file but must NOT register it as read —
        // "read" semantics shouldn't be inferred from a write operation.
        let _ = executor.bash(&serde_json::json!({
            "command": format!("echo hi > {}", file_path.display()),
        }));
        // File was created by the redirect.
        assert!(file_path.exists());
        // …but the read-tracker should still treat it as unread.
        assert!(
            executor.check_staleness(&file_path).is_err(),
            "redirect should not register its target as read"
        );
    }

    #[test]
    fn bash_mixed_command_registers_only_read_segments() {
        // For a compound command like `cat a.rs && echo >> b.rs`, the cat segment
        // genuinely reads a.rs (so the staleness gate must let edits through),
        // while the echo-append segment must NOT register b.rs as read (writes
        // are not reads). Pre-fix code took the whole-command early-return on
        // any mutating segment and dropped a.rs; post-fix relies on
        // `analyze_bash_command` already excluding paths from mutating
        // segments at extraction time.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "content-a\n").unwrap();
        std::fs::write(&b, "content-b\n").unwrap();
        let executor = test_executor_in(dir.path());

        assert!(executor.check_staleness(&a).is_err());
        assert!(executor.check_staleness(&b).is_err());

        let _ = executor.bash(&serde_json::json!({
            "command": format!("cat {} && echo modify >> {}", a.display(), b.display()),
        }));

        assert!(
            executor.check_staleness(&a).is_ok(),
            "cat segment must register {:?} as read",
            a
        );
        // Direct invariant: b.rs was in a mutating segment (echo-append), so
        // register_bash_read_targets must NOT have marked it as read —
        // staleness gate should still block edits on b.rs.
        assert!(
            executor.check_staleness(&b).is_err(),
            "echo-append segment must NOT register {:?} as read",
            b
        );
    }

    #[test]
    fn bash_sed_n_range_read_registers_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("mod.rs");
        std::fs::write(
            &file_path,
            (1..=50).map(|n| format!("line {n}\n")).collect::<String>(),
        )
        .unwrap();
        let executor = test_executor_in(dir.path());

        let _ = executor.bash(&serde_json::json!({
            "command": format!("sed -n '1,20p' {}", file_path.display()),
        }));
        assert!(executor.check_staleness(&file_path).is_ok());
    }

    #[test]
    fn bash_pure_sleep_blocked() {
        let executor = test_executor();
        // Pure sleep without timeout should be blocked
        let result = executor.bash(&serde_json::json!({"command": "sleep 5"}));
        assert!(result.contains("not useful"), "got: {result}");
        // sleep with pipeline work should NOT be blocked
        let result = executor.bash(&serde_json::json!({"command": "sleep 0.01 && echo done"}));
        assert!(result.contains("done"), "got: {result}");
        // sleep with explicit timeout should NOT be blocked (test usage)
        let result = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.1}));
        assert!(result.contains("timed out"), "got: {result}");
    }

    #[test]
    fn bash_git_diff_range_auto_truncated() {
        let executor = test_executor();
        // Multi-commit range diff → auto-piped through head -c, not blocked
        let result = executor
            .bash(&serde_json::json!({"command": "git diff HEAD~5..HEAD 2>/dev/null || true"}));
        // Should NOT contain "built-in" (we no longer hard-block)
        assert!(
            !result.contains("built-in"),
            "should run, not block: {result}"
        );
        // --stat is untouched (no head -c appended)
        let result = executor.bash(
            &serde_json::json!({"command": "git diff HEAD~3..HEAD --stat 2>/dev/null || true"}),
        );
        assert!(
            !result.contains("built-in"),
            "stat should be allowed: {result}"
        );
    }

    #[test]
    fn bash_timeout_kills_process() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.2}));
        assert!(result.contains("timed out"), "got: {result}");
    }

    #[test]
    fn bash_timeout_kills_child_process_tree() {
        // Spawn a parent bash that starts a child sleep.
        // After timeout, verify the child is also killed via process group.
        let executor = test_executor();
        // Use a unique marker file to detect if the child survived
        let marker = format!("/tmp/mo_test_pgid_{}", std::process::id());
        let cmd = format!("bash -c 'sleep 10 && touch {marker}' & wait");
        let result = executor.bash(&serde_json::json!({"command": cmd, "timeout": 0.3}));
        assert!(result.contains("timed out"), "got: {result}");
        // Give a moment for any surviving child to act
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&marker).exists(),
            "child process survived timeout — process group kill failed"
        );
    }

    /// Adaptive bash timeout tiers: instant, fast-read, search, default.
    #[test]
    fn bash_timeout_tiers() {
        // We can't easily test the actual timeout value used internally,
        // but we verify the logic by checking that fast commands complete
        // well within their 5s tier without hitting the 30s default.
        let executor = test_executor();

        // Tier 1 (5s): instant commands
        let start = std::time::Instant::now();
        let r = executor.bash(&serde_json::json!({"command": "echo hello"}));
        assert!(!r.contains("timed out"));
        assert!(start.elapsed().as_secs() < 5);

        // Tier 3 (15s): search command that completes fast
        let r = executor.bash(&serde_json::json!({"command": "grep --version"}));
        assert!(!r.contains("timed out"));

        // Explicit timeout overrides tier
        let r = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.1}));
        assert!(
            r.contains("timed out"),
            "explicit timeout should override tier"
        );
    }

    /// Background commands (with &) should not block indefinitely.
    /// The bash shell exits immediately, but background child processes keep
    /// stdout/stderr pipes open. We must not wait for pipes to close.
    #[test]
    fn bash_background_command_does_not_block() {
        // Tighten the per-pipe drain timeout from 500ms → 50ms so the test
        // runs in <200ms instead of >1s. The invariant under test is "doesn't
        // wait for the backgrounded child to finish"; the absolute drain
        // timeout is not the point.
        let _guard = set_test_bash_pipe_read_timeout_ms(50);
        let executor = test_executor();
        let start = std::time::Instant::now();
        // This command starts a long-running background process and exits immediately.
        // Without the fix, wait_with_output() would block until sleep finishes (60s).
        let result = executor.bash(&serde_json::json!({
            "command": "echo started && sleep 60 &",
            "timeout": 5.0
        }));
        let elapsed = start.elapsed();
        // Must return well before the 60s sleep completes — with the 50ms
        // drain timeout in tests, ~200ms is typical.
        assert!(
            elapsed.as_secs() < 3,
            "background command blocked for {elapsed:?}, should return quickly"
        );
        assert!(
            result.contains("started"),
            "should capture output before background: {result}"
        );
        assert!(
            !result.contains("timed out"),
            "should not timeout: {result}"
        );
    }

    #[test]
    fn bash_failed_command_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo err >&2 && false"}));
        assert!(result.contains("err"), "got: {result}");
    }

    #[test]
    fn powershell_missing_command_returns_error() {
        let executor = test_executor();
        let result = executor.powershell(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn powershell_destructive_warning_detected() {
        let warning = destructive_powershell_warning("Remove-Item -Force temp.txt");
        assert!(warning.is_some());
    }

    #[test]
    fn powershell_path_boundary_blocks_outside_project() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_powershell_path_boundary(&policy, "Get-Content /etc/passwd");
        assert!(result.is_some(), "should block Get-Content /etc/passwd");
        assert!(result.unwrap().starts_with(super::SANDBOX_DENIED_PREFIX));
    }

    #[test]
    fn powershell_echo_returns_output_when_available() {
        let Some(_) = find_powershell_program() else {
            return;
        };
        let executor = test_executor();
        let result = executor.powershell(&serde_json::json!({
            "command": "Write-Output hello"
        }));
        assert!(result.contains("hello"), "got: {result}");
    }

    #[test]
    fn grep_missing_pattern_returns_error() {
        let executor = test_executor();
        let result = executor.grep(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn grep_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "src-tauri/src"
        }));
        assert!(
            result.contains("Error"),
            "should error on missing path, got: {result}"
        );
        assert!(
            result.contains("does not exist"),
            "should mention path doesn't exist, got: {result}"
        );
        assert!(
            result.contains("list_dir"),
            "should suggest list_dir, got: {result}"
        );
    }

    #[test]
    fn grep_nonexistent_absolute_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "/nonexistent/fake/directory"
        }));
        // Sandbox blocks the path before we even check existence
        assert!(
            result.contains("SANDBOX_DENIED") || result.contains("does not exist"),
            "should be blocked by sandbox or report missing path, got: {result}"
        );
    }

    #[test]
    fn grep_finds_pattern_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "foo", "path": "."}));
        assert!(result.contains("foo bar"), "got: {result}");
    }

    #[test]
    fn grep_no_match_returns_message() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "zzzzz", "path": "."}));
        assert!(result.contains("No matches"), "got: {result}");
    }

    #[test]
    fn grep_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("src").join("app.rs"), "needle in source").unwrap();
        std::fs::write(dir.path().join("dist").join("bundle.js"), "needle in build").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "needle", "path": "."}));
        assert!(result.contains("src/app.rs"), "got: {result}");
        assert!(
            !result.contains("dist/bundle.js"),
            "default grep should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("target").join("cached.rs"), "").unwrap();

        let result = executor.glob(&serde_json::json!({"pattern": "*.rs", "path": "."}));
        assert!(result.contains("src/main.rs"), "got: {result}");
        assert!(
            !result.contains("target/cached.rs"),
            "default glob should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.glob(&serde_json::json!({
            "pattern": "*.py",
            "path": "nonexistent/directory"
        }));
        assert!(
            result.contains("Error"),
            "should error on missing path, got: {result}"
        );
        assert!(result.contains("does not exist"), "got: {result}");
        assert!(
            result.contains("list_dir"),
            "should suggest list_dir, got: {result}"
        );
    }

    #[test]
    fn glob_outside_project_sandbox_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.glob(&serde_json::json!({
            "pattern": "*.conf",
            "path": "/etc"
        }));
        assert!(
            result.contains("SANDBOX_DENIED") || result.contains("Sandbox"),
            "glob outside project should be blocked: {result}"
        );
    }

    #[test]
    fn glob_rejects_path_traversal_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        // ".." traversal
        let result = executor.glob(&serde_json::json!({"pattern": "../../etc/*"}));
        assert!(
            result.contains("path traversal"),
            "should reject ..: {result}"
        );
        // Absolute path
        let result = executor.glob(&serde_json::json!({"pattern": "/etc/*.conf"}));
        assert!(
            result.contains("path traversal"),
            "should reject /: {result}"
        );
        // Tilde expansion
        let result = executor.glob(&serde_json::json!({"pattern": "~/.*"}));
        assert!(
            result.contains("path traversal"),
            "should reject ~/: {result}"
        );
        // Normal pattern should work
        let result = executor.glob(&serde_json::json!({"pattern": "*.rs"}));
        assert!(
            !result.contains("path traversal"),
            "should allow *.rs: {result}"
        );
    }

    #[test]
    fn list_dir_outside_project_sandbox_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.list_dir(&serde_json::json!({"path": "/etc"}));
        assert!(
            result.contains("SANDBOX_DENIED") || result.contains("Sandbox"),
            "list_dir outside project should be blocked: {result}"
        );
    }

    // ── str_replace diff preview ─────────────────────────────────────────────

    #[test]
    fn str_replace_shows_diff_preview() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"old\");\n}\n",
        )
        .unwrap();

        executor.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "println!(\"old\")",
            "new_str": "println!(\"new\")"
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(result.contains("- "), "should have - line: {result}");
        assert!(result.contains("+ "), "should have + line: {result}");
        assert!(result.contains("old"), "should show old text: {result}");
        assert!(result.contains("new"), "should show new text: {result}");
    }

    #[test]
    fn str_replace_large_diff_shows_summary() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        // Create a file with many lines
        let old_block: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let new_block: String = (0..25).map(|i| format!("new_line {i}\n")).collect();
        let content = format!("header\n{old_block}footer\n");
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        executor.read_file(&serde_json::json!({"path": "big.txt"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "big.txt",
            "old_str": old_block.trim_end(),
            "new_str": new_block.trim_end()
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(
            result.contains("lines →"),
            "large diff should show summary: {result}"
        );
    }

    // ── resolve_checked sandbox ──────────────────────────────────────────────

    #[test]
    fn resolve_checked_with_permissive_sandbox_allows_all() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        *executor.sandbox_policy.write().unwrap() = Some(SandboxPolicy::permissive(dir.path()));
        let result = executor.resolve_checked("/etc/passwd");
        assert!(result.is_ok(), "should allow with permissive: {result:?}");
    }

    #[test]
    fn resolve_checked_project_relative_path_ok() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.resolve_checked("src/main.rs");
        assert!(
            result.is_ok(),
            "relative path inside project should succeed: {result:?}"
        );
        assert!(result.unwrap().starts_with(dir.path()));
    }

    #[test]
    fn resolve_checked_boundary_violation_has_sandbox_denied_prefix() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        *executor.sandbox_policy.write().unwrap() = Some(SandboxPolicy::for_project(dir.path()));
        let err = executor.resolve_checked("/etc/passwd").unwrap_err();
        assert!(
            err.starts_with(super::SANDBOX_DENIED_PREFIX),
            "boundary violation should have SANDBOX_DENIED prefix: {err}"
        );
    }

    // ── Bash path boundary check ────────────────────────────────────────────

    #[test]
    fn bash_cat_outside_project_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat /etc/passwd");
        assert!(result.is_some(), "should block cat of /etc/passwd");
        assert!(result.unwrap().starts_with(super::SANDBOX_DENIED_PREFIX));
    }

    #[test]
    fn bash_cat_inside_project_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_project(dir.path());
        let result = check_bash_path_boundary(&policy, "cat src/main.rs");
        assert!(result.is_none(), "relative path should be allowed");
    }

    #[test]
    fn bash_cat_tmp_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat /tmp/build.log");
        assert!(result.is_none(), "/tmp should be in allowed_paths");
    }

    #[test]
    fn bash_cat_quoted_space_path_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r#"cat "docs/file with spaces.txt""#);
        assert!(
            result.is_none(),
            "quoted paths should tokenize as one argument"
        );
    }

    #[test]
    fn bash_non_file_command_not_checked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo /etc/passwd");
        assert!(result.is_none(), "echo should not be checked");
    }

    #[test]
    fn bash_grep_not_checked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        // grep is not in the file-access command list (it's a search tool)
        let result = check_bash_path_boundary(&policy, "grep pattern /etc/passwd");
        assert!(
            result.is_none(),
            "grep should not be checked by path boundary"
        );
    }

    #[test]
    fn bash_pipeline_checks_all_commands() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        // cat in the second pipeline stage should also be caught
        let result = check_bash_path_boundary(&policy, "echo hello | cat /etc/passwd");
        assert!(
            result.is_some(),
            "should block cat /etc/passwd even after pipe"
        );
    }

    #[test]
    fn bash_compound_and_checks_all_commands() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "true && cat /etc/passwd");
        assert!(result.is_some(), "should block cat after &&");
    }

    #[test]
    fn bash_semicolon_checks_all_commands() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi; head /etc/shadow");
        assert!(result.is_some(), "should block head after ;");
    }

    #[test]
    fn bash_or_checks_all_commands() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "false || cat /etc/passwd");
        assert!(result.is_some(), "should block cat after ||");
    }

    #[test]
    fn bash_newline_checks_all_commands() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi\ncat /etc/passwd");
        assert!(result.is_some(), "should block cat after newline");
    }

    #[test]
    fn bash_line_continuation_is_not_treated_as_separator() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi\\\ncat /etc/passwd");
        assert!(
            result.is_none(),
            "escaped newline is a line continuation, not a command separator"
        );
    }

    #[test]
    fn bash_full_path_command_detected() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        // /usr/bin/cat should be recognized as "cat"
        let result = check_bash_path_boundary(&policy, "/usr/bin/cat /etc/passwd");
        assert!(result.is_some(), "should detect /usr/bin/cat as cat");
    }

    #[test]
    fn bash_empty_command_no_panic() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        assert!(check_bash_path_boundary(&policy, "").is_none());
        assert!(check_bash_path_boundary(&policy, "   ").is_none());
        assert!(check_bash_path_boundary(&policy, "| ; &&").is_none());
    }

    #[test]
    fn bash_cat_multiple_paths_second_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/tmp");
        // First path is /tmp/ok (allowed), second is /etc/passwd (blocked)
        let result = check_bash_path_boundary(&policy, "cat /tmp/ok /etc/passwd");
        assert!(result.is_some(), "should block second path: /etc/passwd");
        assert!(result.unwrap().contains("/etc/passwd"));
    }

    #[test]
    fn bash_permissive_mode_skips_check() {
        // The check_bash_path_boundary is only called when mode != Permissive
        // (guarded in bash()), but validate_path itself also passes in Permissive.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::permissive("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat /etc/passwd");
        assert!(result.is_none(), "permissive mode should allow everything");
    }

    // ── Bypass attempt tests ────────────────────────────────────────────────
    // These document known bypass vectors and whether they are caught.

    #[test]
    fn bypass_bash_c_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r#"bash -c "cat /etc/passwd""#);
        assert!(
            result.is_some(),
            "nested bash -c file reads should be checked recursively"
        );
    }

    #[test]
    fn bypass_bash_lc_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r#"bash -lc "cat /etc/passwd""#);
        assert!(
            result.is_some(),
            "nested bash -lc file reads should be checked recursively"
        );
    }

    #[test]
    fn bypass_bash_ceu_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r#"bash -ceu "cat /etc/passwd""#);
        assert!(
            result.is_some(),
            "nested bash -ceu file reads should be checked recursively"
        );
    }

    #[test]
    fn bypass_bash_script_path_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "bash /etc/passwd");
        assert!(
            result.is_some(),
            "shell interpreter script paths should be checked recursively"
        );
    }

    #[test]
    fn bypass_bash_option_value_then_script_path_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "bash -O extglob /etc/passwd");
        assert!(
            result.is_some(),
            "option values should not hide later shell script path arguments"
        );
    }

    #[test]
    fn bash_stdin_mode_positional_args_are_not_treated_as_script_paths() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "bash -s /etc/passwd");
        assert!(
            result.is_none(),
            "bash -s reads commands from stdin, so later args are positional only"
        );
    }

    #[test]
    fn bypass_command_substitution_not_caught_by_path_boundary() {
        // $(cat /etc/passwd) — command substitution. Path-boundary parsing does
        // not introspect substitutions, but runtime safety middleware now denies
        // this pattern before execution.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo $(cat /etc/passwd)");
        assert!(
            result.is_none(),
            "command substitution is handled by higher-level shell safety guards"
        );
    }

    #[test]
    fn redirect_input_path_is_caught() {
        // cat < /etc/passwd — spaced redirection target should be checked.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat < /etc/passwd");
        assert!(
            result.is_some(),
            "redirect target path should still be caught"
        );
    }

    #[test]
    fn redirect_input_without_whitespace_is_caught() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat</etc/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "no-space input redirection should still catch the target path"
        );
    }

    #[test]
    fn redirect_output_without_whitespace_on_generic_command_is_caught() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi>/etc/output.log");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/output.log")),
            "redirection scanning should not depend on the outer command allowlist"
        );
    }

    #[test]
    fn redirect_output_and_stderr_path_is_caught() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi >&/etc/output.log");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/output.log")),
            ">&word should be treated as a file path redirection when word is not an fd"
        );
    }

    #[test]
    fn redirect_output_and_stderr_to_fd_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "echo hi >&2");
        assert!(
            result.is_none(),
            ">&2 should remain treated as file-descriptor duplication, not a path access"
        );
    }

    // ── Heredoc body must not be misparsed as redirections ─────────────
    // Regression: HTML/XML/template payloads inside `<< 'EOF' ... EOF` used
    // to trigger SANDBOX_DENIED because tags like `</title>` had their `<`
    // interpreted as a redirection operator with `/title` as the target.

    #[test]
    fn heredoc_body_with_html_tags_is_not_misparsed_as_redirection() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > page.html << 'EOF'\n\
            <!DOCTYPE html>\n\
            <html lang=\"en\"><head><title>hi</title></head>\n\
            <body><a href=\"/admin\">x</a></body></html>\n\
            EOF\n";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(
            result.is_none(),
            "heredoc body with HTML tags must not be treated as redirection: {result:?}"
        );
    }

    #[test]
    fn heredoc_unquoted_delimiter_body_is_skipped() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > out.xml << EOF\n<root><child>/etc/shadow</child></root>\nEOF\n";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(
            result.is_none(),
            "unquoted heredoc must also skip body (no expansion here matters to us): {result:?}"
        );
    }

    #[test]
    fn heredoc_double_quoted_delimiter_body_is_skipped() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > page.svg << \"DONE\"\n<svg><path d=\"M 0 0\"/></svg>\nDONE\n";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(result.is_none(), "double-quoted delimiter: {result:?}");
    }

    #[test]
    fn heredoc_dash_strips_tabs_before_terminator() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        // `<<-` terminator may be preceded by tabs.
        let cmd = "cat > f.html <<-END\n\t<div>a</div>\n\tEND\n";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(result.is_none(), "<<- tab-stripped terminator: {result:?}");
    }

    #[test]
    fn redirection_after_heredoc_body_is_still_caught() {
        // After the heredoc terminator, subsequent redirections must still
        // be validated. Ensures we don't over-skip.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > page.html << 'EOF'\n<html></html>\nEOF\ncat > /etc/passwd";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(
            result.as_deref().is_some_and(|m| m.contains("/etc/passwd")),
            "redirection after heredoc body should still be caught: {result:?}"
        );
    }

    #[test]
    fn heredoc_body_containing_redirection_like_text_does_not_deny() {
        // Body has literal `> /etc/passwd` text — must not be validated as a redir.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > docs.txt << 'EOF'\nusage: foo > /etc/passwd\nEOF\n";
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(
            result.is_none(),
            "body text that *describes* a redirection must not trip sandbox: {result:?}"
        );
    }

    #[test]
    fn heredoc_herestring_is_not_confused_with_heredoc() {
        // `<<<word` is a here-string, NOT a heredoc — the word is inline, no body.
        // Ensure the here-string path isn't accidentally taking the heredoc branch.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "grep foo <<< hello");
        assert!(
            result.is_none(),
            "here-string should not be mis-detected as heredoc: {result:?}"
        );
    }

    #[test]
    fn heredoc_without_terminator_stops_scanning_safely() {
        // Malformed heredoc (no terminator). The validator must not panic;
        // downstream redirections in the tail are unreachable (EOF hit).
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let cmd = "cat > x.html << 'EOF'\n<body>unterminated\n";
        // Should either return None or a safety message — must not panic, must not
        // report `/body` as a denied redirection.
        let result = check_bash_path_boundary(&policy, cmd);
        assert!(
            result
                .as_deref()
                .map(|m| !m.contains("/body"))
                .unwrap_or(true),
            "unterminated heredoc must not surface body chars as a denied path: {result:?}"
        );
    }

    #[test]
    fn redirect_input_without_whitespace_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat<src/main.rs");
        assert!(
            result.is_none(),
            "in-project no-space redirection should remain allowed"
        );
    }

    #[test]
    fn nested_shell_redirect_without_whitespace_is_caught() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "bash -lc 'cat</etc/passwd'");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "nested shell commands should inherit no-space redirection checks"
        );
    }

    #[test]
    fn tilde_expansion_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ~/.ssh/id_rsa");
        assert!(
            result.is_some(),
            "tilde-prefixed paths should be resolved and checked"
        );
    }

    #[test]
    fn tilde_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let project_root = home.join("project");
        let policy = SandboxPolicy::for_project(&project_root);
        let command = "cat ~/project/src/main.rs";
        let result = check_bash_path_boundary(&policy, &command);
        assert!(
            result.is_none(),
            "tilde expansion should still allow paths that stay inside the project"
        );
    }

    #[test]
    fn escaped_tilde_path_is_treated_literally() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r"cat \~/notes.txt");
        assert!(
            result.is_none(),
            "escaped tilde should stay a literal relative path instead of expanding to home"
        );
    }

    #[test]
    fn home_env_expansion_now_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat $HOME/.bashrc");
        assert!(
            result.is_some(),
            "$HOME paths should resolve to the real home dir"
        );
    }

    #[test]
    fn home_env_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let project_root = home.join("project");
        let policy = SandboxPolicy::for_project(&project_root);
        let command = "cat ${HOME}/project/src/main.rs";
        let result = check_bash_path_boundary(&policy, command);
        assert!(
            result.is_none(),
            "$HOME expansion should still allow paths that stay inside the project"
        );
    }

    #[test]
    fn pwd_env_expansion_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat $PWD/../secret.txt");
        assert!(
            result.is_some(),
            "$PWD escapes should be resolved and checked"
        );
    }

    #[test]
    fn pwd_env_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ${PWD}/src/main.rs");
        assert!(
            result.is_none(),
            "$PWD expansion should still allow paths that stay inside the project"
        );
    }

    #[test]
    fn tilde_pwd_expansion_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ~+/../secret.txt");
        assert!(
            result.is_some(),
            "~+ escapes should be resolved and checked"
        );
    }

    #[test]
    fn tilde_pwd_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ~+/src/main.rs");
        assert!(
            result.is_none(),
            "~+ expansion should still allow paths that stay inside the project"
        );
    }

    #[test]
    fn oldpwd_env_expansion_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        use std::path::Path;

        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary_with_oldpwd(
            &policy,
            "cat $OLDPWD/secret.txt",
            Some(Path::new("/etc/previous")),
        );
        assert!(
            result.is_some(),
            "$OLDPWD escapes should be resolved and checked when available"
        );
    }

    #[test]
    fn oldpwd_env_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        use std::path::Path;

        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary_with_oldpwd(
            &policy,
            "cat ${OLDPWD}/note.txt",
            Some(Path::new("/home/user/project/prev")),
        );
        assert!(
            result.is_none(),
            "$OLDPWD expansion should allow paths that stay inside the project"
        );
    }

    #[test]
    fn tilde_oldpwd_expansion_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        use std::path::Path;

        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary_with_oldpwd(
            &policy,
            "cat ~-/secret.txt",
            Some(Path::new("/etc/previous")),
        );
        assert!(
            result.is_some(),
            "~- escapes should be resolved and checked when available"
        );
    }

    #[test]
    fn tilde_oldpwd_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        use std::path::Path;

        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary_with_oldpwd(
            &policy,
            "cat ~-/note.txt",
            Some(Path::new("/home/user/project/prev")),
        );
        assert!(
            result.is_none(),
            "~- expansion should allow paths that stay inside the project"
        );
    }

    #[test]
    fn non_home_env_var_in_path_requires_boundary_review() {
        // cat $TMPDIR/build.log — arbitrary env vars are unresolved at
        // path-boundary time, so they require explicit review instead of being
        // treated as safe literals.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat $TMPDIR/build.log");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("$TMPDIR/build.log")
                    && msg.contains("shell variable expansion")),
            "non-HOME env vars should require boundary review when they cannot be resolved statically"
        );
    }

    #[test]
    fn named_tilde_user_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ~root/.ssh/id_rsa");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("~root/.ssh/id_rsa")
                    && msg.contains("~user home-directory expansion")),
            "~user references should require boundary review"
        );
    }

    #[test]
    fn complex_home_parameter_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ${HOME:-/tmp}/.bashrc");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("${HOME:-/tmp}/.bashrc")
                    && msg.contains("directory anchor")),
            "complex HOME parameter-expansion forms should require boundary review"
        );
    }

    #[test]
    fn complex_pwd_parameter_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ${PWD%/project}/secret.txt");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("${PWD%/project}/secret.txt")
                    && msg.contains("directory anchor")),
            "complex PWD parameter-expansion forms should require boundary review"
        );
    }

    #[test]
    fn complex_oldpwd_parameter_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ${OLDPWD:?missing}/secret.txt");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("${OLDPWD:?missing}/secret.txt")
                    && msg.contains("directory anchor")),
            "complex OLDPWD parameter-expansion forms should require boundary review"
        );
    }

    #[test]
    fn unbraced_variable_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat $SECRET/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("$SECRET/passwd")
                    && msg.contains("shell variable expansion")),
            "unbraced shell variables should require boundary review"
        );
    }

    #[test]
    fn escaped_unbraced_variable_path_is_treated_literally() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r"cat \$SECRET/passwd");
        assert!(
            result.is_none(),
            "escaped shell variables should stay literal paths instead of triggering expansion review"
        );
    }

    #[test]
    fn absolute_unbraced_variable_expansion_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat /$SECRET/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/$SECRET/passwd")
                    && msg.contains("shell variable expansion")),
            "absolute paths containing shell variables should require boundary review"
        );
    }

    #[test]
    fn home_like_variable_name_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat ${HOME_DIR}/notes.txt");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("${HOME_DIR}/notes.txt")
                    && msg.contains("shell variable expansion")),
            "unresolved variable-like anchors should require boundary review"
        );
    }

    #[test]
    fn escaped_home_like_variable_name_is_treated_literally() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, r"cat \${HOME_DIR}/notes.txt");
        assert!(
            result.is_none(),
            "escaped variable syntax should stay literal instead of requiring expansion review"
        );
    }

    #[test]
    fn absolute_home_like_variable_name_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat /${HOME_DIR}/notes.txt");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/${HOME_DIR}/notes.txt")
                    && msg.contains("shell variable expansion")),
            "absolute paths should not bypass unresolved variable review"
        );
    }

    #[test]
    fn process_substitution_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "diff <(cat /etc/passwd) src/main.rs");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "process substitution should recurse into the nested command"
        );
    }

    #[test]
    fn process_substitution_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "diff <(cat src/main.rs) <(cat src/lib.rs)");
        assert!(
            result.is_none(),
            "in-project process substitutions should remain allowed"
        );
    }

    #[test]
    fn brace_expansion_with_outside_path_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat {src/main.rs,/etc/passwd}");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("{src/main.rs,/etc/passwd}")
                    && msg.contains("brace expansion")),
            "brace fan-out should require boundary review when one branch escapes"
        );
    }

    #[test]
    fn brace_expansion_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "cat src/{main,lib}.rs");
        assert!(
            result.is_none(),
            "simple in-project brace expansions should remain allowed"
        );
    }

    #[test]
    fn expanded_command_coverage_blocks_outside_paths() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        for command in [
            "diff src/main.rs /etc/passwd",
            "sort /etc/passwd",
            "awk '{print $1}' /etc/passwd",
            "sed -n '1p' /etc/passwd",
            "echo hi | tee /etc/output.log",
            "cmp src/main.rs /etc/passwd",
            "comm src/main.rs /etc/passwd",
            "join src/main.rs /etc/passwd",
            "cut -d: -f1 /etc/passwd",
            "paste src/main.rs /etc/passwd",
            "uniq /etc/passwd",
        ] {
            let result = check_bash_path_boundary(&policy, command);
            assert!(
                result.is_some(),
                "expanded command coverage should block outside paths for {command}"
            );
        }
    }

    #[test]
    fn expanded_command_coverage_allows_in_project_paths() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        for command in [
            "diff src/main.rs src/lib.rs",
            "sort src/main.rs",
            "awk '{print $1}' src/main.rs",
            "sed -n '1p' src/main.rs",
            "echo hi | tee build/output.log",
            "cmp src/main.rs src/lib.rs",
            "comm src/main.rs src/lib.rs",
            "join src/main.rs src/lib.rs",
            "cut -d: -f1 src/main.rs",
            "paste src/main.rs src/lib.rs",
            "uniq src/main.rs",
        ] {
            let result = check_bash_path_boundary(&policy, command);
            assert!(
                result.is_none(),
                "expanded command coverage should allow in-project paths for {command}"
            );
        }
    }

    #[test]
    fn find_exec_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "find . -name passwd -exec cat {} \\;");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("find -exec cat")),
            "find -exec file fan-out should require boundary review"
        );
    }

    #[test]
    fn find_execdir_shell_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "find . -name passwd -execdir bash -lc 'cat {}' \\;",
        );
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("find -execdir bash")),
            "find -execdir shell fan-out should require boundary review"
        );
    }

    #[test]
    fn find_ok_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "find . -name passwd -ok cat {} \\;");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("find -ok cat")),
            "find -ok file fan-out should require boundary review"
        );
    }

    #[test]
    fn find_exec_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "find . -name rs -exec echo {} +");
        assert!(
            result.is_none(),
            "find fan-out should stay allowed for non-file-access subcommands"
        );
    }

    #[test]
    fn fd_exec_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "fd passwd -x cat {}");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("fd -x cat")),
            "fd -x file fan-out should require boundary review"
        );
    }

    #[test]
    fn fd_exec_batch_shell_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "fd passwd -X bash -lc 'cat {}'");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("fd -X bash")),
            "fd -X shell fan-out should require boundary review"
        );
    }

    #[test]
    fn fd_long_exec_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "fd passwd --exec cat {}");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("fd --exec cat")),
            "fd --exec file fan-out should require boundary review"
        );
    }

    #[test]
    fn fd_exec_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "fd rs -x echo {}");
        assert!(
            result.is_none(),
            "fd fan-out should stay allowed for non-file-access subcommands"
        );
    }

    #[test]
    fn while_read_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "printf '%s\\n' /etc/passwd | while read path; do cat \"$path\"; done",
        );
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("while read cat")),
            "while-read file fan-out should require boundary review"
        );
    }

    #[test]
    fn while_read_shell_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "while IFS= read -r path; do bash -lc 'cat \"$1\"' _ \"$path\"; done < src/files.txt",
        );
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("while read bash")),
            "while-read shell fan-out should require boundary review"
        );
    }

    #[test]
    fn while_read_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "printf '%s\\n' src/main.rs | while read path; do echo \"$path\"; done",
        );
        assert!(
            result.is_none(),
            "while-read loops should stay allowed for non-file-access subcommands"
        );
    }

    #[test]
    fn for_loop_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "for path in src/main.rs /etc/passwd; do cat \"$path\"; done",
        );
        assert!(
            result.as_deref().is_some_and(
                |msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX) && msg.contains("for cat")
            ),
            "for-loop file fan-out should require boundary review"
        );
    }

    #[test]
    fn for_loop_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "for path in src/main.rs src/lib.rs; do echo \"$path\"; done",
        );
        assert!(
            result.is_none(),
            "for-loops should stay allowed for non-file-access subcommands"
        );
    }

    #[test]
    fn generic_shell_loop_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "while true; do cat /etc/passwd; done");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "loop bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn if_then_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "if true; then cat /etc/passwd; fi");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "if bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn if_else_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result =
            check_bash_path_boundary(&policy, "if false; then echo ok; else cat /etc/passwd; fi");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "else bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn if_body_recurses_into_while_read_fanout() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "if true; then printf '%s\\n' /etc/passwd | while read path; do cat \"$path\"; done; fi",
        );
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("while read cat")),
            "compound bodies should recurse into nested loop fan-out checks"
        );
    }

    #[test]
    fn if_then_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "if true; then cat src/main.rs; fi");
        assert!(
            result.is_none(),
            "in-project if bodies should remain allowed"
        );
    }

    #[test]
    fn brace_group_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "{ cat /etc/passwd; echo ok; }");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "brace-group bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn brace_group_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "{ cat src/main.rs; echo ok; }");
        assert!(
            result.is_none(),
            "in-project brace-group bodies should remain allowed"
        );
    }

    #[test]
    fn case_clause_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "case \"$kind\" in passwd) cat /etc/passwd ;; *) echo ok ;; esac",
        );
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "case clause bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn case_clause_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(
            &policy,
            "case \"$kind\" in rs) cat src/main.rs ;; *) echo ok ;; esac",
        );
        assert!(
            result.is_none(),
            "in-project case clause bodies should remain allowed"
        );
    }

    #[test]
    fn subshell_static_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "(cat /etc/passwd)");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "subshell bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn subshell_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "(cat src/main.rs)");
        assert!(
            result.is_none(),
            "in-project subshell bodies should remain allowed"
        );
    }

    #[test]
    fn attached_function_body_outside_path_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "foo(){ cat /etc/passwd; }\nfoo");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("/etc/passwd")),
            "attached function bodies should still run normal path-boundary checks"
        );
    }

    #[test]
    fn attached_function_body_inside_project_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "foo(){ cat src/main.rs; }\nfoo");
        assert!(
            result.is_none(),
            "in-project attached function bodies should remain allowed"
        );
    }

    #[test]
    fn parallel_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "parallel cat ::: /etc/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("parallel cat")),
            "parallel file fan-out should require boundary review"
        );
    }

    #[test]
    fn parallel_shell_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result =
            check_bash_path_boundary(&policy, "parallel bash -lc 'cat {}' ::: src/main.rs");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("parallel bash")),
            "parallel shell fan-out should require boundary review"
        );
    }

    #[test]
    fn parallel_flagged_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "parallel --jobs 4 cat ::: /etc/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("parallel cat")),
            "parallel options with values should not hide the batch subcommand"
        );
    }

    #[test]
    fn parallel_double_dash_file_access_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "parallel -- cat ::: /etc/passwd");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("parallel cat")),
            "parallel -- should still expose the batch subcommand"
        );
    }

    #[test]
    fn parallel_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "parallel echo ::: src/main.rs src/lib.rs");
        assert!(
            result.is_none(),
            "parallel fan-out should stay allowed for non-file-access subcommands"
        );
    }

    #[test]
    fn tar_file_flag_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "tar -xf /etc/archive.tar");
        assert!(
            result.is_some(),
            "tar archive path should be boundary-checked"
        );
    }

    #[test]
    fn tar_directory_flag_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "tar -xf archive.tar -C /etc");
        assert!(
            result.is_some(),
            "tar extraction directory should be boundary-checked"
        );
    }

    #[test]
    fn tar_create_source_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "tar -cf backup.tar /etc/passwd");
        assert!(
            result.is_some(),
            "tar create source operands should be boundary-checked"
        );
    }

    #[test]
    fn tar_old_style_file_flag_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "tar xf /etc/archive.tar");
        assert!(
            result.is_some(),
            "old-style tar option clusters should still validate archive paths"
        );
    }

    #[test]
    fn tar_in_project_paths_are_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "tar -cf backup.tar src");
        assert!(
            result.is_none(),
            "in-project tar paths should remain allowed"
        );
    }

    #[test]
    fn patch_input_file_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "patch -i /etc/fix.patch");
        assert!(
            result.is_some(),
            "patch input files should be boundary-checked"
        );
    }

    #[test]
    fn patch_directory_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "patch -d /etc -i fix.patch");
        assert!(
            result.is_some(),
            "patch working directories should be boundary-checked"
        );
    }

    #[test]
    fn patch_output_file_outside_project_is_blocked() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "patch -o /etc/patched.txt -i fix.patch");
        assert!(
            result.is_some(),
            "patch output files should be boundary-checked"
        );
    }

    #[test]
    fn patch_in_project_paths_are_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "patch -d src -i fixes.patch");
        assert!(
            result.is_none(),
            "in-project patch paths should remain allowed"
        );
    }

    #[test]
    fn xargs_file_access_now_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "find / -name passwd | xargs cat");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("xargs cat")),
            "xargs file fan-out should require boundary review"
        );
    }

    #[test]
    fn xargs_flagged_file_access_still_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result =
            check_bash_path_boundary(&policy, "find / -name passwd -print0 | xargs -0 cat");
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("xargs cat")),
            "xargs flags should not hide file fan-out execution"
        );
    }

    #[test]
    fn xargs_default_echo_is_allowed() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "printf 'src/main.rs\n' | xargs echo");
        assert!(
            result.is_none(),
            "default echo fan-out does not need boundary review"
        );
    }

    #[test]
    fn xargs_shell_now_requires_boundary_review() {
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let command = r#"printf 'src/main.rs\n' | xargs bash -c 'cat "$1"' _"#;
        let result = check_bash_path_boundary(&policy, command);
        assert!(
            result
                .as_deref()
                .is_some_and(|msg| msg.starts_with(super::SANDBOX_DENIED_PREFIX)
                    && msg.contains("xargs bash")),
            "xargs shell fan-out should require boundary review"
        );
    }

    #[test]
    fn bypass_python_read_not_caught_by_path_boundary() {
        // python3 -c "open('/etc/passwd').read()" — interpreter invocation.
        // Path-boundary parsing does not inspect interpreter source code, but
        // higher-level shell safety guards now deny inline interpreter exec.
        use astra_runtime::tool_sandbox::SandboxPolicy;
        let policy = SandboxPolicy::for_project("/home/user/project");
        let result = check_bash_path_boundary(&policy, "python3 -c \"open('/etc/passwd').read()\"");
        assert!(
            result.is_none(),
            "interpreter commands are handled by higher-level shell safety guards"
        );
    }

    // ── SSRF protection ─────────────────────────────────────────────────────

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(is_ssrf_target("http://127.0.0.1:8080/secret").is_some());
        assert!(is_ssrf_target("http://localhost/admin").is_some());
        assert!(is_ssrf_target("http://0.0.0.0:3000").is_some());
        assert!(is_ssrf_target("http://[::1]/api").is_some());
    }

    #[test]
    fn ssrf_blocks_private_networks() {
        assert!(is_ssrf_target("http://10.0.0.1/internal").is_some());
        assert!(is_ssrf_target("http://192.168.1.1/router").is_some());
        assert!(is_ssrf_target("http://172.16.0.1/service").is_some());
        assert!(is_ssrf_target("http://172.31.255.1/db").is_some());
        // 172.15 and 172.32 are NOT private
        assert!(is_ssrf_target("http://172.15.0.1/ok").is_none());
        assert!(is_ssrf_target("http://172.32.0.1/ok").is_none());
    }

    #[test]
    fn ssrf_blocks_cloud_metadata() {
        assert!(is_ssrf_target("http://169.254.169.254/latest/meta-data/").is_some());
        assert!(is_ssrf_target("http://metadata.google.internal/computeMetadata/v1/").is_some());
    }

    #[test]
    fn ssrf_allows_public_urls() {
        assert!(is_ssrf_target("https://github.com/matrixorigin/matrixone").is_none());
        assert!(is_ssrf_target("https://api.github.com/repos").is_none());
        assert!(is_ssrf_target("http://example.com").is_none());
        assert!(is_ssrf_target("https://docs.rs/tokio/latest").is_none());
    }

    // ── web_fetch ─────────────────────────────────────────────────────────────

    #[test]
    fn web_fetch_missing_url_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn web_fetch_invalid_scheme_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({"url": "ftp://example.com"}));
        assert!(result.contains("http"), "got: {result}");
    }

    // ── Process group cleanup tests ──────────────────────────────────────────
    // These tests verify that child processes spawned by grep/glob/curl are
    // properly killed when timing out, preventing zombie process leaks.

    #[test]
    fn run_command_with_cleanup_timeout_kills_process_group() {
        // Test that run_command_with_cleanup properly kills the entire process group
        let marker = format!("/tmp/mo_test_cleanup_{}", std::process::id());
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(format!("sleep 10 && touch {marker}"));

        let result = run_command_with_cleanup(&mut cmd, 0.2);
        assert!(result.is_err(), "should timeout");
        assert!(
            result.unwrap_err().contains("timed out"),
            "should indicate timeout"
        );

        // Give a moment for any surviving child to act
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&marker).exists(),
            "child process survived timeout — process group kill failed"
        );
    }

    #[test]
    fn run_command_with_cleanup_success_returns_output() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let result = run_command_with_cleanup(&mut cmd, 5.0);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[test]
    fn grep_uses_process_group_cleanup() {
        // Verify grep doesn't leave zombie processes on timeout
        // This is a regression test for the curl zombie leak issue.
        // We can't easily force grep to timeout, but we can verify it completes normally.
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "findme\n").unwrap();

        // Normal grep should work
        let result = executor.grep(&serde_json::json!({"pattern": "findme", "path": "."}));
        assert!(result.contains("findme"), "got: {result}");

        // After grep completes, verify no zombie processes from this test
        // (This is more of a smoke test — the real protection is the process_group(0))
    }

    #[test]
    fn glob_uses_process_group_cleanup() {
        // Verify glob (which uses bash internally) properly cleans up
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "content\n").unwrap();

        let result = executor.glob(&serde_json::json!({"pattern": "*.txt"}));
        assert!(result.contains("test.txt"), "got: {result}");
    }

    // ── grep extended regex ──────────────────────────────────────────────────

    #[test]
    fn grep_alternation_pattern_works() {
        // Regression test: grep must use -E for extended regex so that
        // alternation patterns like "foo|bar" work as OR, not literal "|".
        // Session 62c1e8e9: `grep "skill|Skill" --include "*.rs"` returned
        // nothing because without -E, "|" is treated as literal.
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn|struct",
            "include": "*.rs"
        }));
        // In a Rust project, "fn" and "struct" both exist — alternation should match
        assert!(
            !result.contains("No matches found"),
            "Extended regex alternation should work: got: {result}"
        );
    }

    #[test]
    fn grep_basic_pattern_still_works() {
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn main",
            "include": "*.rs"
        }));
        // Simple non-regex pattern should still work
        assert!(!result.is_empty());
    }

    // ── HTML detection ──────────────────────────────────────────────────────

    #[test]
    fn looks_like_html_detects_doctype() {
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>hello</body></html>"
        ));
        assert!(looks_like_html("<!doctype html>\n<html>"));
    }

    #[test]
    fn looks_like_html_detects_html_tag() {
        assert!(looks_like_html("<html lang=\"en\"><head></head></html>"));
        assert!(looks_like_html("<HTML><BODY>hi</BODY></HTML>"));
    }

    #[test]
    fn looks_like_html_rejects_plain_text() {
        assert!(!looks_like_html("Hello world, this is plain text."));
        assert!(!looks_like_html("{\"key\": \"value\"}"));
        assert!(!looks_like_html("# Markdown heading\n\nSome text."));
    }

    #[test]
    fn looks_like_html_rejects_xml_without_body() {
        assert!(!looks_like_html("<root><item>data</item></root>"));
    }

    // ── HTML-to-text conversion ─────────────────────────────────────────────

    #[test]
    fn html_to_text_strips_tags() {
        let html = "<p>Hello <b>world</b></p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("<p>"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn html_to_text_removes_script_and_style() {
        let html = "<html><head><style>body{color:red}</style></head>\
                     <body><script>alert('xss')</script><p>content</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("content"), "got: {text}");
        assert!(!text.contains("alert"), "script not stripped: {text}");
        assert!(!text.contains("color:red"), "style not stripped: {text}");
    }

    #[test]
    fn html_to_text_decodes_entities() {
        let html = "<p>A &amp; B &lt; C &gt; D &quot;E&quot; F&apos;s</p>";
        let text = html_to_text(html);
        assert!(text.contains("A & B"), "got: {text}");
        assert!(text.contains("< C >"), "got: {text}");
        assert!(text.contains("\"E\""), "got: {text}");
        assert!(text.contains("F's"), "got: {text}");
    }

    #[test]
    fn html_to_text_decodes_numeric_entities() {
        let html = "<p>&#65;&#66;&#67;</p>"; // ABC
        let text = html_to_text(html);
        assert!(text.contains("ABC"), "got: {text}");
    }

    #[test]
    fn html_to_text_inserts_newlines_for_blocks() {
        let html = "<h1>Title</h1><p>Paragraph one.</p><p>Paragraph two.</p>";
        let text = html_to_text(html);
        // Block elements should create line breaks
        assert!(text.contains("Title"), "got: {text}");
        assert!(
            text.contains("Paragraph one.") && text.contains("Paragraph two."),
            "got: {text}"
        );
    }

    #[test]
    fn html_to_text_collapses_whitespace() {
        let html = "<p>  lots   of    spaces  </p>\n\n\n\n\n<p>many newlines</p>";
        let text = html_to_text(html);
        assert!(!text.contains("     "), "excessive spaces: {text}");
        // No more than 2 consecutive newlines
        assert!(!text.contains("\n\n\n"), "excessive newlines: {text}");
    }

    #[test]
    fn html_to_text_handles_real_page() {
        let html = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Example</title>
  <style>body { margin: 0; }</style>
  <script>window.ga=function(){}</script>
</head>
<body>
  <div id="main">
    <h1>Welcome</h1>
    <p>This is a <a href="/about">test page</a> with links.</p>
    <ul>
      <li>Item 1</li>
      <li>Item 2</li>
    </ul>
  </div>
  <script src="analytics.js"></script>
</body>
</html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Welcome"), "missing heading: {text}");
        assert!(text.contains("test page"), "missing link text: {text}");
        assert!(text.contains("Item 1"), "missing list item: {text}");
        assert!(!text.contains("<"), "tags not stripped: {text}");
        assert!(!text.contains("window.ga"), "script not removed: {text}");
        assert!(!text.contains("margin: 0"), "style not removed: {text}");
    }

    #[test]
    fn html_to_text_passthrough_json() {
        // JSON is not HTML, so it passes through unchanged
        let json = r#"{"name": "test", "value": 42}"#;
        assert!(!looks_like_html(json));
    }

    #[test]
    fn html_to_text_passthrough_plain_text() {
        let plain = "This is just plain text\nwith some newlines.";
        assert!(!looks_like_html(plain));
    }

    // ── grep context_lines and max_matches ───────────────────────────────────

    #[test]
    fn grep_context_lines_passed_to_command() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ctx.txt");
        std::fs::write(&file, "line1\nline2\nMATCH\nline4\nline5\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "MATCH",
            "path": "ctx.txt",
            "context_lines": 1
        }));
        // With -C1, should see line2 and line4 as context
        assert!(result.contains("MATCH"), "should find match: {result}");
        assert!(
            result.contains("line2") || result.contains("line4"),
            "should have context lines: {result}"
        );
    }

    #[test]
    fn grep_max_matches_limits_output() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repeat.txt");
        std::fs::write(&file, "foo\nfoo\nfoo\nfoo\nfoo\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "foo",
            "path": "repeat.txt",
            "max_matches": 2
        }));
        let match_count = result.matches("foo").count();
        assert!(
            match_count <= 3,
            "should limit to ~2 matches, got {match_count}: {result}"
        );
    }

    #[test]
    fn grep_context_lines_capped_at_10() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("small.txt");
        std::fs::write(&file, "MATCH\n").unwrap();

        let executor = test_executor_in(dir.path());
        // Requesting 100 context lines should be capped to 10
        let result = executor.grep(&serde_json::json!({
            "pattern": "MATCH",
            "path": "small.txt",
            "context_lines": 100
        }));
        assert!(
            result.contains("MATCH"),
            "should still find match: {result}"
        );
    }

    #[test]
    fn grep_combined_context_and_max() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("combo.txt");
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&format!("line{i}\n"));
            if i % 5 == 0 {
                content.push_str("TARGET\n");
            }
        }
        std::fs::write(&file, &content).unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "TARGET",
            "path": "combo.txt",
            "context_lines": 1,
            "max_matches": 2
        }));
        let target_count = result.matches("TARGET").count();
        assert!(
            target_count <= 3,
            "should limit matches, got {target_count}: {result}"
        );
    }

    // ═══════════════════════ Scope Context Tests ═══════════════════════

    #[test]
    fn annotate_grep_with_scope_adds_function_context() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Grep output for a pattern inside a known function in shell.rs itself
        let grep_output = format!(
            "{}/src/edge_tools/shell.rs:10:    use serde_json::Value;",
            root.display()
        );
        let result = annotate_grep_with_scope(&grep_output, root);
        // Should annotate with the containing function/module name
        // (or pass through if tree-sitter can't resolve scope)
        // The key behavior is it doesn't panic and produces output
        assert!(
            !result.is_empty(),
            "should produce non-empty output: {result}"
        );
    }

    #[test]
    fn annotate_grep_with_scope_no_change_for_unknown_files() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let grep_output = "nonexistent.xyz:10:some content";
        let result = annotate_grep_with_scope(grep_output, root);
        assert_eq!(
            result, grep_output,
            "unknown files should pass through unchanged"
        );
    }

    #[test]
    fn annotate_grep_with_scope_handles_empty_input() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = annotate_grep_with_scope("", root);
        assert_eq!(result, "");
    }

    #[test]
    fn annotate_grep_with_scope_preserves_non_match_lines() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let grep_output = "-- separator --\nsome random line";
        let result = annotate_grep_with_scope(grep_output, root);
        assert!(
            result.contains("-- separator --"),
            "should preserve non-match lines"
        );
    }

    #[test]
    fn grep_scope_context_parameter() {
        // Small fixture with a known function — avoids the ~1s overhead of
        // grepping the whole crate source tree under CARGO_MANIFEST_DIR.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sample.rs"),
            "fn annotate_grep_with_scope(input: &str) -> String {\n    String::new()\n}\n",
        )
        .unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn annotate_grep_with_scope",
            "path": "sample.rs",
            "scope_context": true
        }));
        assert!(
            result.contains("annotate_grep_with_scope"),
            "should find the function: {result}"
        );
        // With scope_context=true, should have function annotation
        assert!(
            result.contains("// in "),
            "should have scope context annotation: {result}"
        );
    }

    #[test]
    fn grep_head_limit_truncates_output() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        // Create a file with many matching lines
        let content: String = (0..100).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "head_limit": 5
        }));
        // Should have at most 5 matching lines + metadata
        let match_lines: Vec<&str> = result.lines().filter(|l| l.contains("needle")).collect();
        assert_eq!(
            match_lines.len(),
            5,
            "should limit to 5 lines, got: {result}"
        );
        assert!(
            result.contains("Results limited to"),
            "should note truncation, got: {result}"
        );
    }

    #[test]
    fn grep_head_limit_zero_means_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        let content: String = (0..150).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "head_limit": 0
        }));
        // Should NOT have the "Results limited" message
        assert!(
            !result.contains("Results limited to"),
            "head_limit=0 should be unlimited, got: {result}"
        );
        let match_lines: Vec<&str> = result.lines().filter(|l| l.contains("needle")).collect();
        assert!(
            match_lines.len() > 100,
            "should have all lines, got {}",
            match_lines.len()
        );
    }

    #[test]
    fn grep_default_head_limit_applies() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        // Create more than GREP_DEFAULT_HEAD_LIMIT (100) matching lines
        let content: String = (0..150).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": "."
        }));
        let match_lines: Vec<&str> = result.lines().filter(|l| l.contains("needle")).collect();
        assert_eq!(
            match_lines.len(),
            100,
            "default limit should be 100, got {}",
            match_lines.len()
        );
        assert!(
            result.contains("Results limited to 100"),
            "should note default limit, got: {result}"
        );
    }

    #[test]
    fn grep_offset_with_head_limit() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        let content: String = (0..20).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join("test.txt"), &content).unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "offset": 5,
            "head_limit": 3
        }));
        let match_lines: Vec<&str> = result.lines().filter(|l| l.contains("needle")).collect();
        assert_eq!(
            match_lines.len(),
            3,
            "should have 3 lines after offset, got: {result}"
        );
        // First visible line should be line 5 (0-indexed)
        assert!(
            result.contains("needle line 5"),
            "should start at offset 5, got: {result}"
        );
    }

    #[test]
    fn grep_streaming_preserves_partial_on_timeout() {
        // Test that run_readonly_command_with_partial returns partial stdout on timeout
        let dir = tempfile::tempdir().unwrap();

        // Create a script that outputs lines then hangs
        let script = dir.path().join("slow.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\nfor i in $(seq 1 5); do echo \"match_line_$i\"; done; sleep 5",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script);
        cmd.current_dir(dir.path());

        let (output, _stderr, exit_code, timed_out) =
            super::run_readonly_command_with_partial(&mut cmd, 0.25).expect("should not return Err");
        // Should have captured partial stdout before timeout
        assert!(
            output.contains("match_line_1"),
            "should have partial output, got: {output}"
        );
        assert!(timed_out, "should report timed_out=true");
        assert_eq!(exit_code, -1, "timed out exit code should be -1");
        // Should NOT contain any error metadata in the output (clean stdout only)
        assert!(
            !output.contains("Error:"),
            "output should be clean stdout, got: {output}"
        );
    }

    #[test]
    fn grep_count_mode_with_head_limit() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("a.txt"), "needle\nneedle\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "nothing\n").unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "output_mode": "count",
            "head_limit": 1
        }));
        // Count mode should filter zero-count lines, then apply head_limit.
        // Only count the actual count lines (file:N), not metadata lines.
        let count_lines: Vec<&str> = result.lines().filter(|l| l.contains(".txt:")).collect();
        assert_eq!(
            count_lines.len(),
            1,
            "should limit to 1 count entry, got: {result}"
        );
    }

    #[test]
    fn grep_files_with_matches_mode_works() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("a.txt"), "needle here\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "no match\n").unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "output_mode": "files_with_matches"
        }));
        assert!(
            result.contains("a.txt"),
            "should list matching file, got: {result}"
        );
        assert!(
            !result.contains("b.txt"),
            "should not list non-matching file, got: {result}"
        );
    }

    #[test]
    fn grep_stderr_not_mixed_into_output() {
        // Verify that stderr (e.g. "Binary file matches") doesn't appear in results
        let dir = tempfile::tempdir().unwrap();

        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg("echo 'stdout_line' && echo 'stderr_line' >&2");
        cmd.current_dir(dir.path());

        let (output, _stderr, _exit_code, _timed_out) =
            super::run_readonly_command_with_partial(&mut cmd, 5.0).expect("should not return Err");
        assert!(
            output.contains("stdout_line"),
            "should have stdout, got: {output}"
        );
        assert!(
            !output.contains("stderr_line"),
            "should NOT have stderr, got: {output}"
        );
    }

    #[test]
    fn grep_stderr_captured_separately_for_errors() {
        // Verify stderr is available for error reporting
        let dir = tempfile::tempdir().unwrap();

        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg("echo 'error detail' >&2; exit 2");
        cmd.current_dir(dir.path());

        let (stdout, stderr, exit_code, _) =
            super::run_readonly_command_with_partial(&mut cmd, 5.0).expect("should not return Err");
        assert!(
            stdout.trim().is_empty(),
            "stdout should be empty, got: {stdout}"
        );
        assert!(
            stderr.contains("error detail"),
            "stderr should be captured, got: {stderr}"
        );
        assert_eq!(exit_code, 2);
    }

    #[test]
    fn grep_invalid_regex_reports_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "[invalid",
            "path": "."
        }));
        // Should report the grep error from stderr, not just "grep failed"
        assert!(
            result.starts_with("Error"),
            "should be error, got: {result}"
        );
    }

    #[test]
    fn grep_timeout_with_partial_drops_incomplete_last_line() {
        // When timeout kills grep mid-write, the last line may be incomplete.
        // run_readonly_command_with_partial should drop it.
        let dir = tempfile::tempdir().unwrap();

        let script = dir.path().join("partial.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\necho 'complete_line_1'\necho 'complete_line_2'\nprintf 'incomplete_no_newline'\nsleep 5",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script);
        cmd.current_dir(dir.path());

        let (output, _stderr, _, timed_out) =
            super::run_readonly_command_with_partial(&mut cmd, 0.25).expect("should not return Err");
        assert!(timed_out);
        assert!(
            output.contains("complete_line_1"),
            "should have complete lines, got: {output}"
        );
        assert!(
            output.contains("complete_line_2"),
            "should have complete lines, got: {output}"
        );
        // The incomplete line (no trailing newline) should be dropped
        assert!(
            !output.contains("incomplete_no_newline"),
            "should drop incomplete last line, got: {output}"
        );
    }

    #[test]
    fn grep_timeout_empty_output_returns_actionable_error() {
        // #6 + #14: timeout with zero output → specific error message
        let dir = tempfile::tempdir().unwrap();
        let _executor = super::ToolExecutor::new(dir.path());

        // Create a script that hangs without producing output (simulates grep
        // scanning a huge tree with no matches before timeout)
        let script = dir.path().join("hang.sh");
        std::fs::write(&script, "#!/bin/bash\nsleep 5").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script);
        cmd.current_dir(dir.path());

        let (output, _stderr, _exit, timed_out) =
            super::run_readonly_command_with_partial(&mut cmd, 0.2).expect("should not return Err");
        assert!(timed_out);
        assert!(
            output.trim().is_empty(),
            "should have no output, got: {output}"
        );
    }

    #[test]
    fn grep_no_match_with_stderr_warnings() {
        // #13: exit_code=1 with stderr warnings
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        // Create a binary file that grep will warn about
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 0xFF, 0xFE]).unwrap();
        std::fs::write(dir.path().join("text.txt"), "no match here").unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "zzzzz_nonexistent",
            "path": ".",
            "include": "*"
        }));
        assert!(
            result.contains("No matches"),
            "should report no matches, got: {result}"
        );
    }

    #[test]
    fn grep_offset_beyond_results() {
        // #18: offset >= lines.len()
        let dir = tempfile::tempdir().unwrap();
        let executor = super::ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "needle\n").unwrap();

        let result = executor.grep(&serde_json::json!({
            "pattern": "needle",
            "path": ".",
            "offset": 999
        }));
        assert!(
            result.contains("No more results"),
            "should report no more results, got: {result}"
        );
        assert!(
            result.contains("999"),
            "should mention the offset, got: {result}"
        );
    }

    #[test]
    fn grep_timeout_with_partial_shows_timeout_note() {
        // #24: end-to-end — timed_out with partial results appends timeout note
        let dir = tempfile::tempdir().unwrap();

        // Create many files so grep has something to find before timeout
        for i in 0..20 {
            std::fs::write(
                dir.path().join(format!("f{i}.txt")),
                format!("needle_line_{i}\n"),
            )
            .unwrap();
        }

        // We can't easily make grep itself timeout in a test, so test the
        // metadata appending logic directly: simulate a timed_out result
        // by calling run_readonly_command_with_partial on a slow script
        let script = dir.path().join("slow_grep.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\nfor i in $(seq 1 10); do echo \"file$i.txt:1:needle_$i\"; done; sleep 5",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script);
        cmd.current_dir(dir.path());

        let (output, _stderr, _, timed_out) =
            super::run_readonly_command_with_partial(&mut cmd, 0.25).expect("should not return Err");
        assert!(timed_out);
        assert!(output.contains("needle_1"), "should have partial results");
        // The grep function would append the timeout note — verify the raw
        // output does NOT contain it (clean separation)
        assert!(
            !output.contains("[grep timed out"),
            "raw output should be clean"
        );
    }

    #[test]
    fn readonly_command_caps_output_at_max() {
        // #8: output exceeding MAX_OUTPUT_CHARS is capped
        let dir = tempfile::tempdir().unwrap();

        // Generate output larger than MAX_OUTPUT_CHARS (30_000)
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg("yes 'abcdefghij' | head -5000"); // 5000 * 11 = 55000 chars
        cmd.current_dir(dir.path());

        let (output, _stderr, exit_code, _) =
            super::run_readonly_command_with_partial(&mut cmd, 10.0)
                .expect("should not return Err");
        assert_eq!(exit_code, 0);
        assert!(
            output.len() <= super::MAX_OUTPUT_CHARS + 100, // small margin for partial chunk
            "output should be capped near MAX_OUTPUT_CHARS, got {} bytes",
            output.len()
        );
    }

    // -----------------------------------------------------------------------
    // Command semantics tests
    // -----------------------------------------------------------------------

    #[test]
    fn interpret_grep_exit_1_is_not_error() {
        let r = interpret_exit_code("grep -r foo .", 1);
        assert!(!r.is_error);
        assert_eq!(r.note, Some("No matches found"));
    }

    #[test]
    fn interpret_grep_exit_2_is_error() {
        let r = interpret_exit_code("grep -r foo .", 2);
        assert!(r.is_error);
    }

    #[test]
    fn interpret_diff_exit_1_is_not_error() {
        let r = interpret_exit_code("diff a b", 1);
        assert!(!r.is_error);
    }

    #[test]
    fn interpret_test_exit_1_is_not_error() {
        let r = interpret_exit_code("test -f /tmp/x", 1);
        assert!(!r.is_error);
    }

    #[test]
    fn interpret_pipeline_uses_last_command() {
        // `cat file | grep pattern` — grep is the last command
        let r = interpret_exit_code("cat file | grep pattern", 1);
        assert!(!r.is_error);
        assert_eq!(r.note, Some("No matches found"));
    }

    #[test]
    fn interpret_unknown_command_exit_1_is_error() {
        let r = interpret_exit_code("cargo build", 1);
        assert!(r.is_error);
    }

    // -----------------------------------------------------------------------
    // Destructive command warning tests
    // -----------------------------------------------------------------------

    #[test]
    fn destructive_warning_git_reset_hard() {
        assert!(destructive_command_warning("git reset --hard HEAD~1").is_some());
    }

    #[test]
    fn destructive_warning_git_push_force() {
        assert!(destructive_command_warning("git push --force origin main").is_some());
        assert!(destructive_command_warning("git push -f origin main").is_some());
    }

    #[test]
    fn destructive_warning_safe_commands() {
        assert!(destructive_command_warning("git status").is_none());
        assert!(destructive_command_warning("ls -la").is_none());
        assert!(destructive_command_warning("cargo test").is_none());
    }

    #[test]
    fn destructive_warning_no_verify() {
        assert!(destructive_command_warning("git commit --no-verify -m 'x'").is_some());
    }

    #[test]
    fn forbidden_name_based_process_kill_detects_direct_and_prefixed_forms() {
        assert!(forbidden_name_based_process_kill("pkill -f http.server").is_some());
        assert!(forbidden_name_based_process_kill("sudo pkill -f http.server").is_some());
        assert!(forbidden_name_based_process_kill("cd tmp && killall python3").is_some());
        assert!(forbidden_name_based_process_kill("env FOO=bar pkill node").is_some());
        assert!(forbidden_name_based_process_kill("kill 12345").is_none());
    }

    #[test]
    fn forbidden_name_based_process_kill_detects_absolute_path_bypass() {
        assert!(
            forbidden_name_based_process_kill("/usr/bin/pkill -f http.server").is_some(),
            "absolute path should not bypass pkill block"
        );
        assert!(
            forbidden_name_based_process_kill("sudo /usr/bin/killall python3").is_some(),
            "sudo + absolute path should not bypass killall block"
        );
    }

    #[test]
    fn forbidden_name_based_process_kill_detects_nice_time_strace_prefixes() {
        assert!(
            forbidden_name_based_process_kill("nice pkill -f node").is_some(),
            "nice prefix should not bypass pkill block"
        );
        assert!(
            forbidden_name_based_process_kill("time killall python3").is_some(),
            "time prefix should not bypass killall block"
        );
        assert!(
            forbidden_name_based_process_kill("strace pkill -9 node").is_some(),
            "strace prefix should not bypass pkill block"
        );
        assert!(
            forbidden_name_based_process_kill("exec pkill -f node").is_some(),
            "exec prefix should not bypass pkill block"
        );
    }

    // -----------------------------------------------------------------------
    // Bash integration: command semantics in output
    // -----------------------------------------------------------------------

    #[test]
    fn bash_grep_no_match_not_error() {
        let executor = test_executor();
        let result = executor.bash(
            &serde_json::json!({"command": "grep -r 'ZZZZZ_IMPOSSIBLE_PATTERN_99999' /dev/null"}),
        );
        // Should NOT start with "Error" — grep exit 1 is semantic, not an error
        assert!(
            !result.to_lowercase().starts_with("error"),
            "grep no-match should not be an error: {result}"
        );
    }

    #[test]
    fn bash_false_command_is_error() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "false"}));
        assert!(
            result.contains("exit code") || result.to_lowercase().contains("error"),
            "false should indicate failure: {result}"
        );
    }

    #[test]
    fn bash_destructive_warning_prepended() {
        // Verify the warning function itself works — no need to run actual destructive commands
        let executor = test_executor();
        // Use a command that contains the destructive pattern but is harmless
        let result = executor
            .bash(&serde_json::json!({"command": "echo 'git push --force would be dangerous'"}));
        assert!(
            result.contains("⚠️"),
            "command containing destructive pattern should have warning: {result}"
        );
    }

    #[test]
    fn bash_blocks_name_based_process_kill_commands() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "pkill -f http.server"}));
        assert!(
            result.contains("not allowed in this shared environment"),
            "pkill should be hard-blocked before execution: {result}"
        );
    }

    #[test]
    fn truncate_multibyte_does_not_panic() {
        // Regression: raw truncate at byte offset inside multi-byte char panics.
        let mut s = "café ☕ 你好世界".to_string();
        let limit = 10; // lands inside '☕' (bytes 6..9)
        let boundary = s.floor_char_boundary(limit);
        s.truncate(boundary);
        s.push_str("\n[truncated]");
        assert!(s.starts_with("café "));
    }
}
