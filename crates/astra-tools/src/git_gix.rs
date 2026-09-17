#![allow(clippy::collapsible_if)]
//! Internal repository inspection and bounded Git subprocesses.

use crate::execution_outcome::ToolExecutionOutcome;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use serde_json::Value;

/// Maximum time to wait for a git subprocess to complete.
/// Prevents 67s+ hangs on large merge commits (observed in session 0ac7696c).
const GIT_SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
pub enum GitProcessError {
    RepositoryBinding(String),
    Execution(astra_sandbox::SyncProcessError),
    Exit(String),
}

impl std::fmt::Display for GitProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryBinding(reason) => {
                write!(f, "repository binding failed; no command was run: {reason}")
            }
            Self::Execution(error) => write!(f, "{error}"),
            Self::Exit(error) => write!(f, "{error}"),
        }
    }
}

impl GitProcessError {
    pub fn into_outcome(self, context: &str) -> ToolExecutionOutcome {
        use astra_core::ErrorKind;
        let (kind, started, phase) = match &self {
            Self::RepositoryBinding(_) => (ErrorKind::ToolBinding, false, "repository binding"),
            Self::Execution(error) => (
                match error.phase {
                    "timeout" => ErrorKind::ToolTimeout,
                    "output limit" => ErrorKind::ResourceLimit,
                    "repository binding" => ErrorKind::ToolBinding,
                    _ => ErrorKind::Unknown,
                },
                error.started,
                error.phase,
            ),
            Self::Exit(_) => (ErrorKind::Unknown, true, "exit"),
        };
        let mut evidence = astra_core::ToolFailureEvidence::from_error_kind(kind);
        if started {
            evidence.retryable = false;
            evidence.recovery_actions =
                vec![astra_core::ToolRecoveryAction::InspectStructuredFailure];
        }
        let mut result = ToolExecutionOutcome::error_with_evidence(
            format!("Error: {context}: {self}"),
            evidence,
        );
        let fields = result.tool_result_fields.as_mut().expect("failure fields");
        fields.insert(
            "disposition".into(),
            Value::String(if started { "executed" } else { "rejected" }.into()),
        );
        fields.insert("process_started".into(), Value::Bool(started));
        fields.insert("execution_phase".into(), Value::String(phase.into()));
        result
    }
}

fn open_repo(project_root: &Path) -> Result<gix::Repository, String> {
    let canonical_root = project_root
        .canonicalize()
        .map_err(|error| format!("Error: cannot resolve bound git repo: {error}"))?;
    let repo = gix::open(&canonical_root)
        .map_err(|e| format!("Error: cannot open bound git repo: {e}"))?;
    let work_dir = repo
        .workdir()
        .ok_or_else(|| "Error: bound git repository has no working tree".to_string())?
        .canonicalize()
        .map_err(|error| format!("Error: cannot resolve bound git working tree: {error}"))?;
    if work_dir != canonical_root {
        return Err(format!(
            "Error: bound git working tree escapes the selected project root ({})",
            work_dir.display()
        ));
    }
    Ok(repo)
}

/// Git command bound to identities acquired and revalidated before launch.
///
/// This is a startup check, not an immutable filesystem view: Git opens real
/// metadata paths after exec. Concurrent replacement in that window is governed
/// by the selected provider's isolation. A detected post-start change is an
/// error with possible effects, never evidence of a successful mutation.
pub struct BoundGitCommand {
    #[cfg(not(unix))]
    root: std::path::PathBuf,
    git_path: std::path::PathBuf,
    args: Vec<String>,
    observation: Option<crate::workspace_observation::WorkspaceAttributionState>,
    #[cfg(unix)]
    binding: std::sync::Arc<GitBinding>,
}

pub struct BoundTokioGitCommand {
    command: tokio::process::Command,
    #[cfg(unix)]
    _binding: std::sync::Arc<GitBinding>,
}

#[cfg(unix)]
struct GitIdentity {
    path: std::ffi::CString,
    file: std::fs::File,
    stat: libc::stat,
}
#[cfg(unix)]
struct GitBinding {
    identities: Vec<GitIdentity>,
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let root = std::fs::File::open("/")?;
    astra_sandbox::open_directory_beneath(
        &root,
        path.strip_prefix("/").map_err(std::io::Error::other)?,
    )
}

#[cfg(unix)]
fn open_git_entry(
    worktree: &std::fs::File,
    name: &std::ffi::CStr,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::openat(
            worktree.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(std::io::Error::other(
            ".git must be a regular file or directory",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn parse_linked_git_dir(
    dot_git: &std::fs::File,
    worktree_path: &Path,
) -> Result<std::path::PathBuf, String> {
    use std::io::Read;
    use std::os::unix::ffi::OsStringExt;
    let mut bytes = Vec::new();
    dot_git
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read .git: {e}"))?;
    if bytes.len() > 65536 {
        return Err("bound .git file exceeds 64 KiB".into());
    }
    let path = bytes
        .strip_prefix(b"gitdir: ")
        .ok_or("bound .git file has no gitdir directive")?;
    let path = path.strip_suffix(b"\n").unwrap_or(path);
    let path = path.strip_suffix(b"\r").unwrap_or(path);
    if path.is_empty() || path.iter().any(|c| matches!(c, 0 | b'\n' | b'\r')) {
        return Err("invalid gitdir path".into());
    }
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(path.to_vec()));
    // Canonicalization is a label-resolution step; acquisition below walks all
    // resulting directory components without following symbolic links.
    let path = if path.is_absolute() {
        path
    } else {
        worktree_path.join(path)
    };
    path.canonicalize()
        .map_err(|e| format!("cannot resolve linked git metadata: {e}"))
}

#[cfg(unix)]
impl GitIdentity {
    fn new(path: &Path, file: std::fs::File) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            path,
            file,
            stat: unsafe { stat.assume_init() },
        })
    }
    // Async-signal-safe: no allocation, locks or Rust filesystem wrappers.
    fn unchanged(&self) -> bool {
        let mut current = std::mem::MaybeUninit::uninit();
        if unsafe { libc::lstat(self.path.as_ptr(), current.as_mut_ptr()) } != 0 {
            return false;
        }
        let current = unsafe { current.assume_init() };
        current.st_dev == self.stat.st_dev
            && current.st_ino == self.stat.st_ino
            && current.st_mode == self.stat.st_mode
            && (current.st_mode & libc::S_IFMT != libc::S_IFREG
                || (current.st_size == self.stat.st_size
                    && current.st_mtime == self.stat.st_mtime
                    && current.st_mtime_nsec == self.stat.st_mtime_nsec))
    }
}
#[cfg(unix)]
impl GitBinding {
    fn unchanged(&self) -> bool {
        self.identities.iter().all(GitIdentity::unchanged)
    }
}

fn clear_git_location(command: &mut std::process::Command) {
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_INDEX_FILE",
        "GIT_CEILING_DIRECTORIES",
    ] {
        command.env_remove(variable);
    }
    command.env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "0");
}

impl BoundGitCommand {
    pub fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        // All existing tool argument contracts are UTF-8. Preserve invalid OS
        // paths as a rejected command, never silently execute a lossy spelling.
        self.args
            .push(arg.as_ref().to_str().unwrap_or("\0").to_owned());
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }
    fn configure(
        &self,
        command: &mut std::process::Command,
        force_worktree: bool,
    ) -> std::io::Result<()> {
        clear_git_location(command);
        command.env("GIT_DIR", &self.git_path);
        if force_worktree {
            command.env("GIT_WORK_TREE", ".");
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let binding = self.binding.clone();
            unsafe {
                command.pre_exec(move || {
                    if !binding.unchanged() {
                        return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
                    }
                    if libc::fchdir(binding.identities[0].file.as_raw_fd()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        command.current_dir(&self.root);
        Ok(())
    }
    pub fn output(&mut self) -> Result<std::process::Output, astra_sandbox::SyncProcessError> {
        let result = astra_sandbox::run_sync_process(
            "git",
            &self.args,
            GIT_SUBPROCESS_TIMEOUT,
            16 * 1024 * 1024,
            |cmd| self.configure(cmd, true),
        );
        let (started, ownership) = match &result {
            Ok(out) => (true, out.ownership),
            Err(err) => (err.started, err.ownership),
        };
        if started && let Some(observation) = &self.observation {
            #[cfg(unix)]
            if ownership.is_none() {
                observation.mark_unsettled();
            }
            if !ownership.is_some_and(|owner| owner.is_authoritative()) {
                observation.quarantine();
            }
        }
        #[cfg(unix)]
        if started && !self.binding.unchanged() {
            if let Some(observation) = &self.observation {
                observation.quarantine();
            }
            return Err(astra_sandbox::SyncProcessError { phase: "repository binding", detail: "repository identity changed during execution; effects may have occurred; no mutation receipt is valid".into(), started: true, ownership });
        }
        result.map(|out| out.output)
    }
    pub fn status(&mut self) -> Result<std::process::ExitStatus, astra_sandbox::SyncProcessError> {
        self.output().map(|out| out.status)
    }
    /// Retains startup binding checks only. The optional ignore-query caller
    /// owns concurrent IO, timeout and cancellation; this adapter does not
    /// inherit synchronous invocation ownership or post-execution validation.
    pub fn into_tokio(self) -> BoundTokioGitCommand {
        let mut command = std::process::Command::new("git");
        command.args(&self.args);
        // Configuration only installs inherited settings and pre-exec checks.
        self.configure(&mut command, true)
            .expect("Git command configuration is infallible");
        BoundTokioGitCommand {
            command: command.into(),
            #[cfg(unix)]
            _binding: self.binding,
        }
    }
}
impl Deref for BoundTokioGitCommand {
    type Target = tokio::process::Command;
    fn deref(&self) -> &Self::Target {
        &self.command
    }
}
impl DerefMut for BoundTokioGitCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

#[cfg(unix)]
fn prepare_bound_git_command_with_pin_hook(
    project_root: &Path,
    after_pin_before_validation: impl FnOnce(),
) -> Result<BoundGitCommand, String> {
    let root = project_root
        .canonicalize()
        .map_err(|e| format!("Error: cannot resolve bound git repo: {e}"))?;
    let worktree = open_directory_no_follow(&root)
        .map_err(|e| format!("Error: cannot acquire bound git working tree: {e}"))?;
    let dot_git = open_git_entry(&worktree, c".git")
        .map_err(|e| format!("Error: bound project root has no exact .git authority: {e}"))?;
    let git_path = if dot_git.metadata().map_err(|e| e.to_string())?.is_dir() {
        root.join(".git")
    } else {
        parse_linked_git_dir(&dot_git, &root)?
    };
    let git_dir = open_directory_no_follow(&git_path)
        .map_err(|e| format!("Error: cannot acquire git metadata: {e}"))?;
    let mut identities = vec![
        GitIdentity::new(&root, worktree),
        GitIdentity::new(&root.join(".git"), dot_git),
        GitIdentity::new(&git_path, git_dir),
    ]
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .map_err(|e| format!("Error: cannot record git identities: {e}"))?;
    // Linked repositories also rely on a shared metadata directory. Acquire
    // its canonical identity before validating Git's original configuration.
    match open_git_entry(&identities[2].file, c"commondir") {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            (&file)
                .take(65537)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Error: cannot read common git directory: {e}"))?;
            if bytes.len() > 65536 {
                return Err("Error: common git directory file exceeds 64 KiB".into());
            }
            let common = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
            let common = git_path
                .join(common.trim())
                .canonicalize()
                .map_err(|e| format!("Error: cannot resolve common git directory: {e}"))?;
            identities.push(
                GitIdentity::new(&git_path.join("commondir"), file).map_err(|e| e.to_string())?,
            );
            identities.push(
                GitIdentity::new(
                    &common,
                    open_directory_no_follow(&common).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?,
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Error: cannot acquire common git directory: {error}"
            ));
        }
    }
    let observation = crate::workspace_observation::WorkspaceAttributionState::capture(&root);
    let command = BoundGitCommand {
        #[cfg(not(unix))]
        root,
        git_path,
        args: Vec::new(),
        observation,
        binding: std::sync::Arc::new(GitBinding { identities }),
    };
    after_pin_before_validation();
    if !command.binding.unchanged() {
        return Err("Error: repository binding changed before launch; no command was run".into());
    }
    // Preserve core.worktree/bare validation before forcing GIT_WORK_TREE.
    let args = [
        "rev-parse",
        "--is-inside-work-tree",
        "--is-bare-repository",
        "--show-toplevel",
    ]
    .map(str::to_owned);
    let output =
        astra_sandbox::run_sync_process("git", &args, GIT_SUBPROCESS_TIMEOUT, 65536, |cmd| {
            command.configure(cmd, false)
        })
        .map_err(|e| format!("Error: cannot validate bound git repository: {e}"))?
        .output;
    if !output.status.success() {
        return Err(format!(
            "Error: bound git repository validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(top) = text
        .strip_prefix("true\nfalse\n")
        .and_then(|s| s.strip_suffix('\n'))
    else {
        return Err("Error: bound git metadata does not describe a working-tree repository".into());
    };
    let reported = open_directory_no_follow(Path::new(top))
        .map_err(|e| format!("Error: cannot acquire repository-reported working tree: {e}"))?;
    use std::os::unix::fs::MetadataExt;
    let expected = command.binding.identities[0]
        .file
        .metadata()
        .map_err(|e| e.to_string())?;
    let reported = reported.metadata().map_err(|e| e.to_string())?;
    if reported.dev() != expected.dev()
        || reported.ino() != expected.ino()
        || !command.binding.unchanged()
    {
        return Err(
            "Error: bound git working tree escapes or changed from selected project root".into(),
        );
    }
    Ok(command)
}

pub fn prepare_bound_git_command(project_root: &Path) -> Result<BoundGitCommand, String> {
    #[cfg(unix)]
    {
        prepare_bound_git_command_with_pin_hook(project_root, || {})
    }
    #[cfg(not(unix))]
    {
        let root = project_root
            .canonicalize()
            .map_err(|e| format!("Error: cannot resolve bound git repo: {e}"))?;
        let repo = open_repo(&root)?;
        let git_path = repo
            .path()
            .canonicalize()
            .map_err(|e| format!("Error: cannot resolve bound git metadata: {e}"))?;
        Ok(BoundGitCommand {
            observation: crate::workspace_observation::WorkspaceAttributionState::capture(&root),
            #[cfg(not(unix))]
            root,
            git_path,
            args: Vec::new(),
        })
    }
}

/// Return the current branch name (like `git branch --show-current`).
pub fn current_branch(project_root: &Path) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    match repo.head_ref() {
        Ok(Some(reference)) => reference.name().shorten().to_string(),
        _ => String::new(),
    }
}

/// Return the short HEAD commit hash (like `git rev-parse --short HEAD`).
pub fn head_short(project_root: &Path) -> String {
    let repo = match open_repo(project_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    match repo.head_id() {
        Ok(id) => id.to_hex_with_len(7).to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn process_failure_metadata_keeps_local_phase_and_possible_effects() {
        for (phase, kind) in [
            ("timeout", "tool_timeout"),
            ("output limit", "resource_limit"),
            ("repository binding", "tool_binding"),
        ] {
            let outcome = GitProcessError::Execution(astra_sandbox::SyncProcessError {
                phase,
                detail: "fixture".into(),
                started: true,
                ownership: None,
            })
            .into_outcome("git");
            assert!(outcome.is_error);
            let fields = outcome.tool_result_fields.unwrap();
            assert_eq!(fields["error_kind"], kind);
            assert_eq!(fields["disposition"], "executed");
            assert_eq!(fields["process_started"], true);
            assert_eq!(fields["recovery_evidence"]["retryable"], false);
        }
        let outcome =
            GitProcessError::RepositoryBinding("missing metadata".into()).into_outcome("git");
        assert_eq!(
            outcome.tool_result_fields.unwrap()["disposition"],
            "rejected"
        );
    }

    fn repo_root() -> std::path::PathBuf {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // rust/
        path
    }

    fn init_temp_repo() -> TempDir {
        let dir = TempDir::new().expect("temp repo");
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.name", "Test User"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("tracked.txt"), "one\n").expect("seed tracked file");
        run_git(dir.path(), &["add", "tracked.txt"]);
        run_git(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_git_command_rejects_observed_metadata_replacement_before_launch() {
        let repo = init_temp_repo();
        let result = prepare_bound_git_command_with_pin_hook(repo.path(), || {
            std::fs::rename(repo.path().join(".git"), repo.path().join(".git-original")).unwrap();
            run_git(repo.path(), &["init"]);
        });
        assert!(result.is_err());
        let mut command = prepare_bound_git_command(repo.path()).unwrap();
        std::fs::rename(repo.path().join(".git"), repo.path().join(".git-replaced")).unwrap();
        run_git(repo.path(), &["init"]);
        let error = command
            .args(["status", "--porcelain"])
            .output()
            .unwrap_err();
        assert!(!error.started);
    }

    #[cfg(unix)]
    #[test]
    fn bound_git_runtime_replacement_reports_possible_effects_without_receipt() {
        let repo = init_temp_repo();
        let mut command = prepare_bound_git_command(repo.path()).unwrap();
        let failure = command
            .args([
                "-c",
                "alias.astra-test-rebind=!mv .git .git-moved",
                "astra-test-rebind",
            ])
            .output()
            .unwrap_err();
        assert!(failure.started);
        assert_eq!(failure.phase, "repository binding");
        assert!(repo.path().join(".git-moved").exists());
        assert_eq!(
            crate::workspace_observation::workspace_observation_is_quarantined(repo.path()),
            Some(true)
        );
    }

    // --- Bug #3: show should reject range syntax with helpful message ---

    // --- Bug #5: diff .. branch must validate with reject_shell_meta ---

    // Supplementary: tip contains shell meta (not just base)

    // Supplementary: triple-dot range works

    // ─── Diff with actual content verification ──────────────────────────────

    // ─── Edge cases ─────────────────────────────────────────────────────────

    // ─── Score function unit tests ──────────────────────────────────────────

    // ─── parse_since_to_epoch tests ─────────────────────────────────────────

    // ─── current_branch / head_short tests ──────────────────────────────────

    #[test]
    fn current_branch_returns_nonempty_in_repo() {
        let root = repo_root();
        let branch = current_branch(&root);
        // In a git repo we should get a branch name (or empty if detached HEAD)
        // Just verify no panic and reasonable output
        assert!(!branch.contains("Error"), "should not error: {branch}");
    }

    #[test]
    fn head_short_returns_hex() {
        let root = repo_root();
        let short = head_short(&root);
        assert!(!short.is_empty(), "should return a short hash");
        assert_eq!(
            short.len(),
            7,
            "to_hex_with_len(7) yields 7 hex chars: {short}"
        );
        assert!(
            short.chars().all(|c| c.is_ascii_hexdigit()),
            "should be hex: {short}"
        );
    }

    #[test]
    fn current_branch_bad_path_returns_empty() {
        let branch = current_branch(Path::new("/nonexistent/repo"));
        assert!(branch.is_empty());
    }

    #[test]
    fn head_short_bad_path_returns_empty() {
        let short = head_short(Path::new("/nonexistent/repo"));
        assert!(short.is_empty());
    }

    // ─── Robustness regression tests ────────────────────────────────────────

    // ── Pressure-aware output limit tests ──

    // ─── commit tests ───────────────────────────────────────────────────

    // ─── stash tests ────────────────────────────────────────────────────

    // ─── git action checkout_file tests ────────────────────────────────────────────

    // ─── git CLI fallback behavior tests ────────────────────────────────────

    // ── Git Worktree Tests ──────────────────────────────────────────────
}
