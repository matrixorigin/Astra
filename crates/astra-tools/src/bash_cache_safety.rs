//! Classify bash commands by cache safety.
//!
//! Used by [`DefaultToolExecutor`](crate::executor::DefaultToolExecutor)'s
//! per-session dedup to decide which commands may return a cached
//! result. The **only** cache-safe commands are purely-readonly
//! queries whose output depends solely on: the filesystem state, the
//! cwd, and process environment (captured separately via env
//! fingerprinting).
//!
//! ## Design: fail-closed
//!
//! The classifier is a strict allowlist. An unknown first-token is
//! **not cache-safe** by default — caching a command whose side
//! effects we haven't audited means replaying a stale `rm -rf` output
//! while the real filesystem has already moved on. That failure mode
//! is catastrophic; the inverse (missing a cache hit on a harmless
//! command) is merely a small perf regression.
//!
//! The classifier also outright denies any command that contains
//! shell metacharacters linking multiple statements (`&&`, `||`, `;`,
//! `|`, `>`, `>>`, `<`), command substitution (`$(...)`, backticks),
//! or background execution (`&`). Reasoning about combined commands
//! requires parsing shell, which we don't — so we don't cache them at
//! all, `force=true` stays the escape hatch.

/// Return true when the command is safe to return from the dedup
/// cache on a repeat invocation. Conservative by design — see module
/// docs.
///
/// Accepts the raw command string as the tool would pass to `bash
/// -c`. Leading whitespace is tolerated. Empty input returns false.
#[must_use]
pub fn bash_command_is_cache_safe(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }

    // Shell compound / redirection disqualifies outright. Detecting
    // `&&` before `&` matters so a backgrounded `cmd &` isn't misread
    // as `&&`. Order below is significant.
    const COMPOUND_MARKERS: &[&str] = &["&&", "||", ";", "|", ">", "<", "$(", "`", "\n"];
    if COMPOUND_MARKERS.iter().any(|m| cmd.contains(m)) {
        return false;
    }
    // Bare `&` at end means background exec — treat as unsafe.
    if cmd.ends_with('&') {
        return false;
    }

    // Strip leading env-var assignments: `FOO=bar LANG=C ls -la` →
    // classify `ls -la`. We do not hash the env here (caller handles
    // that), so changes to those assignments would hit the cache
    // with a different env fingerprint. Separately correct.
    let mut words = cmd.split_whitespace().collect::<Vec<_>>();
    while words.first().is_some_and(|tok| is_env_assignment(tok)) {
        words.remove(0);
    }

    let Some(first) = words.first().copied() else {
        return false;
    };
    let second = words.get(1).copied();
    if known_mutating_shape(&words, first, second) {
        return false;
    }
    let mut trailing = words.iter().skip(2).copied().peekable();

    // Hyphenated tool heads (`git`, `cargo`, etc.) use the first
    // subcommand to disambiguate — `git log` is safe, `git commit`
    // is not.
    match first {
        // Plain read-only tools. Single-token commands like `pwd` are
        // always safe.
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "file" | "stat" | "echo" | "printf"
        | "which" | "whereis" | "type" | "env" | "printenv" | "date" | "uname" | "hostname"
        | "whoami" | "id" | "uptime" | "df" | "du" | "tree" => true,

        // Search tools: safe. Their output depends on fs state, which
        // is already in the cache key via workspace_generation +
        // cwd. Users who change files outside the tool path get
        // stale output; the session TTL is the backstop.
        "find" | "grep" | "rg" | "ripgrep" | "ag" | "fd" | "fdfind" => true,

        // Version queries: only safe as a BARE two-token command
        // (`node --version`, `python3 -V`). Trailing arguments could
        // repurpose the tool as a real runner that incidentally
        // prints its version first, and we can't prove the output
        // still depends purely on installed version.
        "node" | "npm" | "pnpm" | "yarn" | "python" | "python3" | "pip" | "pip3" | "ruby"
        | "go" | "deno" | "bun" | "rustc" | "rustup" | "java" | "javac" | "mvn" | "gradle"
        | "kubectl" | "docker" | "podman"
            if (second == Some("--version") || second == Some("-V"))
                && trailing.next().is_none() =>
        {
            true
        }

        // git subcommands: only the read-only ones are safe.
        "git" => {
            matches!(
                second,
                Some("status")
                | Some("log")
                | Some("diff")
                | Some("show")
                | Some("branch")
                | Some("rev-parse")
                | Some("config")
                | Some("remote")
                | Some("tag")
                | Some("stash")  // list mode common; non-list writes
                | Some("describe")
                | Some("blame")
                | Some("ls-files")
                | Some("reflog")
                | Some("cat-file")
                | Some("fsck")
                | Some("grep")
                | Some("help")
                | Some("--version")
            ) && !git_subcommand_is_dangerous(&cmd.to_ascii_lowercase())
        }

        // cargo: only metadata / tree / check-style reads. build,
        // test, run, publish, install all mutate target/ or network.
        //
        // `-v`/`-V`/`--version` are version queries ONLY when they
        // stand alone. `cargo -v build` / `cargo --verbose build`
        // is a real BUILD with verbose output, not a query — these
        // must NOT be cache-safe.
        "cargo" => match second {
            Some("metadata")
            | Some("tree")
            | Some("pkgid")
            | Some("locate-project")
            | Some("search") => true,
            Some("-V") | Some("-v") | Some("--version") => trailing.next().is_none(),
            _ => false,
        },

        _ => false,
    }
}

fn is_env_assignment(tok: &str) -> bool {
    let mut chars = tok.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    let mut saw_eq = false;
    for c in chars {
        if c == '=' {
            saw_eq = true;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    saw_eq
}

/// A small, provider-neutral supplement to the command-family allowlist.
/// Some tools are read-only only for a particular option shape; treating the
/// family name alone as safe would both replay stale output and skip the
/// executor's mutation observation window. Unknown shapes remain unsafe.
fn known_mutating_shape(words: &[&str], first: &str, second: Option<&str>) -> bool {
    match first {
        "env" => words.len() > 1,
        "find" | "fd" | "fdfind" => words.iter().skip(1).any(|word| {
            matches!(
                *word,
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
                    | "-x"
                    | "--exec"
                    | "--exec-batch"
            )
        }),
        "git" => match second {
            Some("stash") => !matches!(words.get(2).copied(), Some("list" | "show")),
            Some("config") => {
                let args = &words[2..];
                if args.is_empty() {
                    true
                } else if args.iter().any(|arg| {
                    matches!(
                        *arg,
                        "--get"
                            | "--get-all"
                            | "--get-regexp"
                            | "--list"
                            | "-l"
                            | "--show-origin"
                            | "--show-scope"
                            | "--name-only"
                    )
                }) {
                    false
                } else {
                    args.len() > 1
                }
            }
            Some("branch") => words.get(2).is_some_and(|arg| !arg.starts_with('-')),
            Some("tag") => words.get(2).is_some_and(|arg| !arg.starts_with('-')),
            Some("remote") => words.get(2).is_some_and(|arg| {
                !matches!(*arg, "-v" | "--verbose" | "show" | "get-url" | "get")
            }),
            _ => false,
        },
        _ => false,
    }
}

/// Extra sieve for git subcommands: even within the read-only
/// subcommand list, some combinations actively mutate state (e.g.
/// `git stash push`, `git config --set`). Returns true on any
/// detected mutation marker.
fn git_subcommand_is_dangerous(lower: &str) -> bool {
    // `git stash list/show` are observations; bare `git stash` defaults to
    // push, and the other subcommands mutate.
    if lower.starts_with("git stash ")
        && !lower.starts_with("git stash list")
        && !lower.starts_with("git stash show")
    {
        return true;
    }
    // `git config --set`, `--unset`, `--add`, `--rename-section`,
    // `--remove-section` mutate.
    if lower.starts_with("git config ") {
        for frag in [
            "--set",
            "--unset",
            "--add",
            "--rename-section",
            "--remove-section",
        ] {
            if lower.contains(frag) {
                return true;
            }
        }
    }
    // `git remote add`, `remove`, `rename`, `set-url` mutate.
    if lower.starts_with("git remote ") {
        for frag in [" add ", " remove ", " rename ", " set-url "] {
            if lower.contains(frag) {
                return true;
            }
        }
    }
    // `git branch -d/-D`, `--delete`, `--move`, `--copy` mutate.
    if lower.starts_with("git branch ") {
        for frag in [" -d", " -d ", " --delete", " --move", " --copy"] {
            if lower.contains(frag) {
                return true;
            }
        }
    }
    // `git tag -d/-D`, `--delete`, and any `git tag <name>` form
    // without `-l`/`--list` creates. Conservatively deny anything
    // other than `git tag -l` / `git tag --list`.
    if lower.starts_with("git tag ") && !lower.contains(" -l") && !lower.contains(" --list") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::bash_command_is_cache_safe;

    // ── Allowlist: these MUST be cacheable ─────────────────────────

    #[test]
    fn cache_safe_cases() {
        for cmd in [
            // simple read tools
            "ls",
            "ls -la",
            "pwd",
            "cat /etc/hostname",
            "head -n 5 foo",
            "wc -l bar.txt",
            "file /bin/ls",
            "which cargo",
            "uname -a",
            "date",
            // search tools
            "grep -rn foo .",
            "rg 'pattern' src/",
            "find . -name '*.rs'",
            "fd -e rs",
            // git readonly
            "git status",
            "git log --oneline -20",
            "git diff HEAD~1",
            "git show HEAD",
            "git branch -a",
            "git rev-parse HEAD",
            "git stash list",
            "git config user.name",
            "git remote -v",
            // version queries
            "cargo --version",
            "node --version",
            "rustc --version",
            "python3 --version",
            "cargo -V",
            "cargo -v",
            "cargo --version",
            // cargo metadata
            "cargo metadata --format-version 1",
            "cargo tree",
            // env prefix passthrough (real command determines safety)
            "LANG=C ls",
            "FOO=1 BAR=2 pwd",
        ] {
            assert!(
                bash_command_is_cache_safe(cmd),
                "expected cache-safe: {cmd}"
            );
        }

        // bare version query (no subcommand) is safe
        assert!(bash_command_is_cache_safe("node --version"));
        assert!(bash_command_is_cache_safe("python3 --version"));
    }

    #[test]
    fn not_cache_safe_cases() {
        for cmd in [
            // cargo -v/-V followed by subcommand is NOT cache-safe (actually builds/tests)
            "cargo -v build",
            "cargo -v test",
            "cargo -v run",
            "cargo -V build",
            "cargo --verbose build",
            "cargo -v --release build",
            // mutating commands
            "rm -rf /tmp/foo",
            "rm file.txt",
            "mv a b",
            "cp a b",
            "mkdir new",
            "touch file",
            "chmod +x script.sh",
            "chown user file",
            "ln -s a b",
            "tar -czf out.tgz src",
            "dd if=/dev/zero of=file",
            // network commands
            "curl https://example.com",
            "wget https://example.com",
            "ssh host echo hi",
            "scp file host:",
            "git clone https://github.com/a/b",
            "git fetch",
            "git pull",
            "git push",
            // build and install commands
            "cargo build",
            "cargo test",
            "cargo run",
            "cargo install foo",
            "cargo publish",
            "npm install",
            "npm run build",
            "make",
            "make clean",
            "rustup update",
            "pip install foo",
            // git mutating subcommands
            "git commit -m msg",
            "git checkout main",
            "git reset --hard",
            "git rebase main",
            "git merge feat",
            "git cherry-pick abc",
            "git stash push",
            "git stash pop",
            "git stash save msg",
            "git config --set user.name X",
            "git config user.name X",
            "git remote add origin url",
            "git remote remove origin",
            "git branch -d feat",
            "git branch --delete feat",
            "git tag v1",
            "git tag -d v1",
            "git branch new-feature",
            "git stash",
            "find . -delete",
            "find . -exec touch {} \\;",
            "fd -x rm {}",
            "env find . -delete",
            "env python3 -c 'open(\"out\", \"w\").write(\"x\")'",
            // compound/redirect commands
            "ls && cat foo",
            "ls || echo fail",
            "ls ; pwd",
            "ls | grep foo",
            "ls > out.txt",
            "ls >> out.txt",
            "cat < in.txt",
            "echo $(pwd)",
            "echo `pwd`",
            "sleep 1 &",
            "ls\npwd",
            // unknown commands (fail-closed)
            "totally-unknown-tool",
            "my_script.sh",
            "./run-this",
            "python some_script.py",
            "bash -c 'echo hi'",
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "must NOT be cache-safe: {cmd}"
            );
        }

        // empty/whitespace
        assert!(!bash_command_is_cache_safe(""));
        assert!(!bash_command_is_cache_safe("   "));
        assert!(!bash_command_is_cache_safe("\n"));

        // version query with trailing args → NOT cache-safe (can't prove query mode)
        assert!(!bash_command_is_cache_safe("node --version extra"));
        assert!(!bash_command_is_cache_safe("python3 --version foo.py"));

        // env prefix on denylisted command → still NOT cache-safe
        assert!(!bash_command_is_cache_safe("FOO=1 rm file"));
    }
}
