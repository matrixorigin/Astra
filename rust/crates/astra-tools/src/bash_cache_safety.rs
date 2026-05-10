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
    let mut tokens = cmd.split_whitespace().peekable();
    while let Some(tok) = tokens.peek() {
        if is_env_assignment(tok) {
            tokens.next();
        } else {
            break;
        }
    }

    let Some(first) = tokens.next() else {
        return false;
    };

    let second = tokens.next();

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

        // Version queries: always safe.
        "node" | "npm" | "pnpm" | "yarn" | "python" | "python3" | "pip" | "pip3" | "ruby"
        | "go" | "deno" | "bun" | "rustc" | "rustup" | "cargo" | "java" | "javac" | "mvn"
        | "gradle" | "kubectl" | "docker" | "podman"
            if second == Some("--version") || second == Some("-V") =>
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
        "cargo" => matches!(
            second,
            Some("metadata")
                | Some("tree")
                | Some("pkgid")
                | Some("locate-project")
                | Some("search")
                | Some("--version")
                | Some("-V")
                | Some("-v")
        ),

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

/// Extra sieve for git subcommands: even within the read-only
/// subcommand list, some combinations actively mutate state (e.g.
/// `git stash push`, `git config --set`). Returns true on any
/// detected mutation marker.
fn git_subcommand_is_dangerous(lower: &str) -> bool {
    // `git stash` alone is a list; `git stash push` / `save` / `pop`
    // are mutations.
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
    use super::*;

    // ── Allowlist: these MUST be cacheable ─────────────────────────

    #[test]
    fn simple_read_tools_are_cache_safe() {
        for cmd in [
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
        ] {
            assert!(
                bash_command_is_cache_safe(cmd),
                "expected cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn search_tools_are_cache_safe() {
        for cmd in [
            "grep -rn foo .",
            "rg 'pattern' src/",
            "find . -name '*.rs'",
            "fd -e rs",
        ] {
            assert!(
                bash_command_is_cache_safe(cmd),
                "expected cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn git_readonly_subcommands_are_cache_safe() {
        for cmd in [
            "git status",
            "git log --oneline -20",
            "git diff HEAD~1",
            "git show HEAD",
            "git branch -a",
            "git rev-parse HEAD",
            "git stash list",
            "git config user.name",
            "git remote -v",
        ] {
            assert!(
                bash_command_is_cache_safe(cmd),
                "expected cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn version_queries_are_cache_safe() {
        for cmd in [
            "cargo --version",
            "node --version",
            "rustc --version",
            "python3 --version",
        ] {
            assert!(
                bash_command_is_cache_safe(cmd),
                "expected cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn cargo_metadata_is_cache_safe() {
        assert!(bash_command_is_cache_safe(
            "cargo metadata --format-version 1"
        ));
        assert!(bash_command_is_cache_safe("cargo tree"));
    }

    // ── Denylist: these MUST NOT be cacheable ──────────────────────

    #[test]
    fn mutating_commands_are_not_cache_safe() {
        for cmd in [
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
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "must NOT be cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn network_commands_are_not_cache_safe() {
        for cmd in [
            "curl https://example.com",
            "wget https://example.com",
            "ssh host echo hi",
            "scp file host:",
            "git clone https://github.com/a/b",
            "git fetch",
            "git pull",
            "git push",
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "must NOT be cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn build_and_install_commands_are_not_cache_safe() {
        for cmd in [
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
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "must NOT be cache-safe: {cmd}"
            );
        }
    }

    #[test]
    fn git_mutating_subcommands_are_not_cache_safe() {
        for cmd in [
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
            "git remote add origin url",
            "git remote remove origin",
            "git branch -d feat",
            "git branch --delete feat",
            "git tag v1",
            "git tag -d v1",
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "must NOT be cache-safe: {cmd}"
            );
        }
    }

    // ── Shell compound / redirection disqualifies outright ─────────

    #[test]
    fn compound_commands_are_not_cache_safe() {
        for cmd in [
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
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "compound/redirect must NOT be cache-safe: {cmd}"
            );
        }
    }

    // ── Unknown tools default to NOT cache-safe (fail-closed) ──────

    #[test]
    fn unknown_commands_are_not_cache_safe() {
        for cmd in [
            "totally-unknown-tool",
            "my_script.sh",
            "./run-this",
            "python some_script.py",
            "bash -c 'echo hi'",
        ] {
            assert!(
                !bash_command_is_cache_safe(cmd),
                "unknown command must NOT be cache-safe (fail-closed): {cmd}"
            );
        }
    }

    #[test]
    fn empty_and_whitespace_are_not_cache_safe() {
        assert!(!bash_command_is_cache_safe(""));
        assert!(!bash_command_is_cache_safe("   "));
        assert!(!bash_command_is_cache_safe("\n"));
    }

    // ── Env var prefix handling ────────────────────────────────────

    #[test]
    fn env_prefix_passes_to_classifier_for_the_real_command() {
        // env-var assignments are stripped before lookup, so
        // `LANG=C ls` classifies via `ls`.
        assert!(bash_command_is_cache_safe("LANG=C ls"));
        assert!(bash_command_is_cache_safe("FOO=1 BAR=2 pwd"));
        // But a denylisted command stays denied even with env prefix.
        assert!(!bash_command_is_cache_safe("FOO=1 rm file"));
    }
}
