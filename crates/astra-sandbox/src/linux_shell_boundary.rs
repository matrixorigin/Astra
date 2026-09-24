//! Linux restricted-root launch preparation, not a process executor.
//!
//! Requires bubblewrap >= 0.11 (--bind-fd identity checks and JSON exec proof),
//! Linux close_range(CLOEXEC), and the mandatory namespaces/seccomp. Unsupported
//! kernels fail closed. The host must exclusively allocate the writable tree:
//! no external writers, shared hardlinks, FIFOs/devices, or nested mounts. Read
//! roots are trusted, host-selected toolchain inputs, not model-provided paths.
//! Native file confinement remains the file executor's responsibility.
use crate::{ShellProcessBoundary, open_directory_beneath};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Command, ExitStatus, Stdio};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Single-use confinement preparation. Does not implement process ownership.
///
/// The invocation owner must preserve its pre_exec hook and settle descendants
/// on every outcome. `into_supervised_command` retains the launch descriptors
/// through the canonical supervisor's exec boundary; do not flatten a plan to
/// argv. Linux tuple wrapping remains unavailable.
#[derive(Debug)]
pub struct ShellLaunchPlan {
    args: Vec<OsString>,
    inherited: Vec<File>,
    status: File,
}

/// Private bubblewrap status channel. A child exit code alone is not evidence
/// that confinement setup succeeded. This is not a descendant-settlement token.
#[derive(Debug)]
pub struct ShellLaunchReceipt {
    status: File,
}

impl ShellLaunchPlan {
    /// Freeze this identity alongside the explicit system/toolchain manifest.
    /// Protected workspace directories are explicit launch inputs.
    pub const PROFILE_ID: &'static str = astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE;

    /// Consume the plan into the target command and its independent setup
    /// receipt. stdin defaults to /dev/null, stdout/stderr to pipes. Only pipes
    /// and /dev/null may be installed as stdio (enforced after fork).
    ///
    /// The command retains all launch FDs, even if the plan is dropped. Drop
    /// the Command immediately after spawn, then call receipt.verify after the
    /// owner has waited and settled. This method never starts a process.
    pub fn into_command(self) -> (Command, ShellLaunchReceipt) {
        let mut command = Command::new("/usr/bin/bwrap");
        command.args(&self.args);
        self.configure_command(command)
    }

    /// Prepare the canonical invocation supervisor with this plan's retained
    /// descriptors. Call owner.install after command configuration and before
    /// spawning, then owner.started and authoritative settlement as usual.
    /// Only trusted launcher configuration may be applied to this command.
    pub fn into_supervised_command(
        self,
    ) -> io::Result<(Command, crate::BashInvocationOwner, ShellLaunchReceipt)> {
        let args = self.supervisor_args()?;
        let (command, owner) =
            crate::BashInvocationOwner::prepare_supervised("/usr/bin/bwrap", &args)?;
        let (command, receipt) = self.configure_command(command);
        Ok((command, owner, receipt))
    }

    fn supervisor_args(&self) -> io::Result<Vec<String>> {
        self.args
            .iter()
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("supervised confinement requires UTF-8 arguments"))
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn into_supervised_test_command(
        self,
    ) -> io::Result<(Command, crate::BashInvocationOwner, ShellLaunchReceipt)> {
        let (command, owner) = crate::BashInvocationOwner::prepare_with_supervisor_helper(
            std::env::current_exe()?,
            vec![
                "--exact".into(),
                "linux_shell_boundary::tests::supervisor_helper".into(),
                "--nocapture".into(),
            ],
            "/usr/bin/bwrap",
            &self.supervisor_args()?,
        )?;
        let (command, receipt) = self.configure_command(command);
        Ok((command, owner, receipt))
    }

    fn configure_command(self, mut command: Command) -> (Command, ShellLaunchReceipt) {
        command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let inherited = self.inherited;
        // SAFETY: this hook uses only async-signal-safe syscalls and retained
        // immutable descriptors. CLOEXEC is changed only in the forked child;
        // concurrent parent spawns can never inherit the boundary's handles.
        unsafe {
            command.pre_exec(move || {
                for fd in 0..3 {
                    let mut stat: libc::stat = std::mem::zeroed();
                    if libc::fstat(fd, &mut stat) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let pipe = stat.st_mode & libc::S_IFMT == libc::S_IFIFO;
                    let null = stat.st_mode & libc::S_IFMT == libc::S_IFCHR
                        && libc::major(stat.st_rdev) == 1
                        && libc::minor(stat.st_rdev) == 3;
                    if !pipe && !null {
                        return Err(io::Error::from_raw_os_error(libc::EPERM));
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Mark, do not close: Rust's exec-error pipe must stay usable
                // if a later pre_exec hook or exec fails.
                if libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                for file in &inherited {
                    if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        (
            command,
            ShellLaunchReceipt {
                status: self.status,
            },
        )
    }
}

impl ShellLaunchReceipt {
    /// Verify completed bubblewrap setup/exec and agreement with its exit
    /// status. Missing, incomplete, malformed, or conflicting evidence is an
    /// infrastructure failure even when the exit status matches a verifier's
    /// expected failure code. Nonblocking, bounded, single-use; call after wait.
    pub fn verify(mut self, status: ExitStatus) -> io::Result<i32> {
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.status)
            .take(4097)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(invalid("oversized confinement receipt"));
        }
        let mut documents =
            serde_json::Deserializer::from_slice(&bytes).into_iter::<serde_json::Value>();
        let info = documents
            .next()
            .transpose()
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("confinement setup did not complete"))?;
        if !info
            .get("child-pid")
            .and_then(|v| v.as_u64())
            .is_some_and(|pid| pid > 0)
        {
            return Err(invalid("missing confinement child identity"));
        }
        let exit = documents
            .next()
            .transpose()
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("confinement setup/exec did not complete"))?;
        let code = exit
            .get("exit-code")
            .and_then(|v| v.as_i64())
            .filter(|code| (0..=255).contains(code))
            .ok_or_else(|| invalid("invalid confined exit code"))? as i32;
        if documents.next().is_some() || status.code() != Some(code) {
            return Err(invalid(
                "confinement receipt disagrees with process outcome",
            ));
        }
        Ok(code)
    }
}

fn pin(path: &Path) -> io::Result<File> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(invalid("mount roots must be absolute and normalized"));
    }
    open_directory_beneath(&File::open("/")?, path.strip_prefix("/").unwrap())
}

fn sealed_policy() -> io::Result<File> {
    let bytes = crate::shell_seccomp::policy()?;
    // Anonymous, close-on-exec from creation, and immutable before inheritance.
    let fd = unsafe {
        libc::memfd_create(
            c"astra-shell-seccomp".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&bytes)?;
    file.rewind()?;
    if unsafe {
        libc::fcntl(
            fd,
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

pub(crate) fn prepare(
    boundary: &ShellProcessBoundary,
    cwd: &Path,
    program: &str,
    args: &[String],
    protected_paths: &[std::path::PathBuf],
) -> io::Result<ShellLaunchPlan> {
    if boundary.workspace == Path::new("/") {
        return Err(invalid("filesystem root cannot be a workspace"));
    }
    let workspace = pin(&boundary.workspace)?;
    inspect_tree(&workspace, true)?;
    let working = pin(cwd)?;
    let w = workspace.metadata()?;
    let c = working.metadata()?;
    if (w.dev(), w.ino()) != (c.dev(), c.ino()) {
        return Err(invalid("restricted profile requires workspace-root cwd"));
    }
    if !Path::new(program).is_absolute() {
        return Err(invalid("restricted program must be an absolute guest path"));
    }
    let mut argv: Vec<OsString> = [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup",
        "--disable-userns",
        "--assert-userns-disabled",
        "--new-session",
        "--die-with-parent",
        "--cap-drop",
        "ALL",
        "--clearenv",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut inherited = Vec::new();
    let mut roots = Vec::new();
    for path in &boundary.read_only_paths {
        // Mount destinations cannot overlap each other or private guest roots.
        // Refuse broad ambient roots even when explicitly supplied.
        let destination = path
            .to_str()
            .ok_or_else(|| invalid("toolchain path is not UTF-8"))?;
        astra_runtime_env::validate_confined_toolchain_mount(destination)
            .map_err(|message| invalid(&message))?;
        if path.starts_with(&boundary.workspace)
            || boundary.workspace.starts_with(path)
            || roots
                .iter()
                .any(|p: &&Path| path.starts_with(p) || p.starts_with(path))
        {
            return Err(invalid("overlapping, ambient, or reserved toolchain root"));
        }
        let file = pin(path)?;
        inspect_tree(&file, false)?;
        argv.extend([
            "--ro-bind-fd".into(),
            file.as_raw_fd().to_string().into(),
            path.as_os_str().into(),
        ]);
        inherited.push(file);
        roots.push(path.as_path());
    }
    argv.extend([
        "--bind-fd".into(),
        workspace.as_raw_fd().to_string().into(),
        "/workspace".into(),
    ]);
    for protected in protected_paths {
        let relative = protected
            .strip_prefix(&boundary.workspace)
            .map_err(|_| invalid("protected directory is outside workspace"))?;
        if relative.components().count() != 1
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid(
                "protected directory must be a direct workspace child",
            ));
        }
        let source = open_directory_beneath(&workspace, relative)?;
        argv.extend([
            "--ro-bind-fd".into(),
            source.as_raw_fd().to_string().into(),
            Path::new("/workspace").join(relative).into_os_string(),
        ]);
        inherited.push(source);
    }
    inherited.push(workspace);
    // These aliases expose nothing unless their explicit /usr targets exist.
    for (source, destination) in [
        ("usr/bin", "/bin"),
        ("usr/sbin", "/sbin"),
        ("usr/lib", "/lib"),
        ("usr/lib64", "/lib64"),
    ] {
        argv.extend(["--symlink".into(), source.into(), destination.into()]);
    }
    for (option, path) in [
        ("--proc", "/proc"),
        ("--dev", "/dev"),
        ("--tmpfs", "/tmp"),
        ("--tmpfs", "/home"),
        ("--dir", "/home/sandbox"),
    ] {
        argv.extend([option.into(), path.into()]);
    }
    for (key, value) in boundary.environment() {
        argv.extend(["--setenv".into(), key.into(), value]);
    }
    let policy = sealed_policy()?;
    argv.extend(["--seccomp".into(), policy.as_raw_fd().to_string().into()]);
    inherited.push(policy);
    let mut pipe = [-1; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let status = unsafe { File::from_raw_fd(pipe[0]) };
    let writer = unsafe { File::from_raw_fd(pipe[1]) };
    argv.extend([
        "--json-status-fd".into(),
        writer.as_raw_fd().to_string().into(),
    ]);
    inherited.push(writer);
    argv.extend([
        "--chdir".into(),
        "/workspace".into(),
        "--".into(),
        program.into(),
    ]);
    argv.extend(args.iter().map(Into::into));
    if status.as_raw_fd() < 3 || inherited.iter().any(|file| file.as_raw_fd() < 3) {
        return Err(invalid("launch descriptors must not overlap standard IO"));
    }
    Ok(ShellLaunchPlan {
        args: argv,
        inherited,
        status,
    })
}

// Admission of the host-owned allocation, not the socket security boundary.
// External writers must be excluded by allocation ownership throughout launch
// and execution. The child cannot create FIFOs/devices/mounts under seccomp.
fn inspect_tree(root: &File, writable: bool) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    fn mount_id(file: &File) -> io::Result<u64> {
        let mut stat: libc::statx = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                &mut stat,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(invalid("kernel cannot attest mount identity"));
        }
        Ok(stat.stx_mnt_id)
    }
    fn walk(
        dir: &File,
        mount: u64,
        writable: bool,
        budget: &mut usize,
        depth: usize,
    ) -> io::Result<()> {
        if depth > 128 {
            return Err(invalid("mount input is too deep"));
        }
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let entry = entry?;
            *budget = budget
                .checked_sub(1)
                .ok_or_else(|| invalid("mount input is too large"))?;
            let name = std::ffi::CString::new(entry.file_name().as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if mount_id(&file)? != mount {
                return Err(invalid("nested mounts are not admitted"));
            }
            let metadata = file.metadata()?;
            match metadata.mode() & libc::S_IFMT {
                libc::S_IFDIR => walk(&file, mount, writable, budget, depth + 1)?,
                libc::S_IFREG if !writable || metadata.nlink() == 1 => {}
                // Sockets are deliberately admitted: syscall enforcement,
                // including alternate ABIs, must make them unusable.
                libc::S_IFLNK | libc::S_IFSOCK => {}
                _ => {
                    return Err(invalid(
                        "shared hardlinks, FIFOs and devices are not admitted",
                    ));
                }
            }
        }
        Ok(())
    }
    walk(root, mount_id(root)?, writable, &mut 1_000_000, 0)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, net::UnixListener, process::ExitStatusExt};

    fn fixture(root: &Path) -> ShellProcessBoundary {
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        ShellProcessBoundary {
            workspace,
            // Deliberately not mounted: Linux uses fresh anonymous storage.
            home: root.join("host-home"),
            temp: root.join("host-temp"),
            read_only_paths: vec![],
        }
    }

    fn toolchains(boundary: &mut ShellProcessBoundary) {
        boundary.read_only_paths = ["/usr/bin", "/usr/lib", "/usr/lib64"]
            .into_iter()
            .map(Into::into)
            .collect();
    }

    fn plan(boundary: &ShellProcessBoundary, program: &str, args: &[String]) -> ShellLaunchPlan {
        boundary
            .launch_plan(&boundary.workspace, program, args)
            .unwrap()
    }

    fn run(plan: ShellLaunchPlan) -> (std::process::Output, io::Result<i32>) {
        let (mut command, receipt) = plan.into_command();
        let child = command.spawn().unwrap();
        drop(command);
        let output = child.wait_with_output().unwrap();
        let proof = receipt.verify(output.status);
        (output, proof)
    }

    // These exercise the public executor, including the real supervisor and
    // launcher. Namespace restrictions are failures, never skipped successes.
    fn execution_config(root: &Path) -> crate::IsolationConfig {
        let mut config = crate::IsolationConfig::strict(root.join("missing-host-cwd"));
        config.memory_limit_bytes = 0;
        config.cpu_quota = 0.0;
        config.timeout = std::time::Duration::from_secs(10);
        // All legacy path inputs must be ignored for the confined plan.
        config.read_only_paths = vec![root.join("missing-protected-path")];
        config.pinned_working_dir = Some(std::sync::Arc::new(File::open("/").unwrap()));
        config
    }

    fn assert_verified(output: &crate::ConfinedOutput, code: i32) {
        assert_eq!(
            output.confinement,
            crate::ShellConfinementEvidence::LinuxRestrictedRootV1 { receipt: Ok(code) },
            "confinement unavailable or failed: {output:?}"
        );
        assert_eq!(output.process.exit_code, Some(code), "{output:?}");
        assert!(output.process.namespace_active, "{output:?}");
        assert!(output.process.scope_settled, "{output:?}");
        assert_eq!(
            output.process.scope_ownership,
            Some(crate::ScopeOwnership::InvocationSupervisor)
        );
    }

    #[tokio::test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    async fn public_confined_success_nonzero_environment_and_protection() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let metadata = boundary.workspace.join(".git");
        std::fs::create_dir(&metadata).unwrap();
        std::fs::write(metadata.join("config"), "trusted").unwrap();
        let config = execution_config(root.path());
        for code in [0, 7] {
            let script = format!(
                r#"
                test "$HOME" = /home/sandbox || exit 21
                test "$TMPDIR" = /tmp || exit 22
                test "$PWD" = /workspace || exit 23
                test -z "$BASH_ENV$LD_PRELOAD$SSH_AUTH_SOCK" || exit 24
                if echo bad > .git/config; then exit 25; fi
                echo success
                echo diagnostic >&2
                exit {code}
            "#
            );
            let plan = boundary
                .launch_plan_with_protected_paths(
                    &boundary.workspace,
                    "/bin/sh",
                    &["-c".into(), script],
                    std::slice::from_ref(&metadata),
                )
                .unwrap();
            let output = crate::execute_confined_with_cancel(plan, &config, None).await;
            assert_verified(&output, code);
            assert!(output.process.stdout.contains("success"), "{output:?}");
            assert!(output.process.stderr.contains("diagnostic"), "{output:?}");
            assert_eq!(
                std::fs::read_to_string(metadata.join("config")).unwrap(),
                "trusted"
            );
        }
        // Exact guest environment, including absence of host variables and
        // the invocation supervisor's private protocol keys.
        let mut expected: std::collections::BTreeMap<_, _> = boundary
            .environment()
            .into_iter()
            .map(|(key, value)| (key, value.into_string().unwrap()))
            .collect();
        expected.insert("PWD", "/workspace".into());
        let script = format!(
            "import os,json; assert dict(os.environ) == json.loads({:?}), dict(os.environ); assert os.getcwd() == '/workspace'",
            serde_json::to_string(&expected).unwrap()
        );
        let output = crate::execute_confined_with_cancel(
            plan(&boundary, "/usr/bin/python3", &["-c".into(), script]),
            &config,
            None,
        )
        .await;
        assert_verified(&output, 0);
    }

    #[tokio::test]
    #[ignore = "system test: requires bubblewrap 0.11"]
    async fn public_confined_setup_failure_has_no_verifier_exit() {
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        let output = crate::execute_confined_with_cancel(
            plan(&boundary, "/missing-program", &[]),
            &execution_config(root.path()),
            None,
        )
        .await;
        assert!(output.process.execution_started, "{output:?}");
        assert!(output.process.exit_code.is_none(), "{output:?}");
        assert!(!output.process.namespace_active, "{output:?}");
        assert!(output.process.scope_settled, "{output:?}");
        assert!(matches!(
            output.confinement,
            crate::ShellConfinementEvidence::LinuxRestrictedRootV1 { receipt: Err(_) }
        ));
    }

    #[tokio::test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    async fn public_confined_cancel_and_timeout_settle() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        for cancel in [true, false] {
            let ready = boundary.workspace.join("ready");
            let _ = std::fs::remove_file(&ready);
            let mut config = execution_config(root.path());
            if !cancel {
                config.timeout = std::time::Duration::from_secs(2);
            }
            let token = tokio_util::sync::CancellationToken::new();
            let run = crate::execute_confined_with_cancel(
                plan(
                    &boundary,
                    "/bin/sh",
                    &["-c".into(), "sleep 30 & echo ready > ready; wait".into()],
                ),
                &config,
                Some(&token),
            );
            tokio::pin!(run);
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    tokio::select! {
                        output = &mut run => panic!("confinement failed before ready: {output:?}"),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                            if ready.exists() { break; }
                        }
                    }
                }
            })
            .await
            .expect("confined child ready");
            if cancel {
                token.cancel();
            }
            let output = run.await;
            assert_eq!(output.process.cancelled, cancel, "{output:?}");
            assert_eq!(output.process.timed_out, !cancel, "{output:?}");
            assert!(output.process.exit_code.is_none(), "{output:?}");
            assert!(output.process.scope_settled, "{output:?}");
            assert_eq!(
                output.process.scope_ownership,
                Some(crate::ScopeOwnership::InvocationSupervisor)
            );
            assert!(matches!(
                output.confinement,
                crate::ShellConfinementEvidence::LinuxRestrictedRootV1 { receipt: Err(_) }
            ));
        }
    }

    #[tokio::test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    async fn public_confined_output_bounds_and_descendant_settlement() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let mut config = execution_config(root.path());
        config.max_output_bytes = 128;
        let script =
            "(sleep 1; echo escaped > late-marker) >/dev/null 2>&1 & printf '%01024d\\n' 0; exit 0";
        let output = crate::execute_confined_with_cancel(
            plan(&boundary, "/bin/sh", &["-c".into(), script.into()]),
            &config,
            None,
        )
        .await;
        assert_verified(&output, 0);
        assert!(output.process.stdout_capped, "{output:?}");
        assert!(output.process.stdout.len() + output.process.stderr.len() <= 128);
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(!boundary.workspace.join("late-marker").exists());
    }

    #[test]
    fn rejects_ambient_roots_aliases_subdirectory_cwd_and_unsafe_objects() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        assert!(boundary.wrap("/bin/true", &[]).is_err());
        for path in ["/", "/usr", "/home", "/tmp", "/proc", "/dev", "/etc"] {
            boundary.read_only_paths = vec![path.into()];
            assert!(
                boundary
                    .launch_plan(&boundary.workspace, "/bin/true", &[])
                    .is_err()
            );
        }
        boundary.read_only_paths.clear();
        std::fs::create_dir(boundary.workspace.join("nested")).unwrap();
        assert!(
            boundary
                .launch_plan(&boundary.workspace.join("nested"), "/bin/true", &[])
                .is_err()
        );
        symlink(&boundary.workspace, root.path().join("alias")).unwrap();
        boundary.read_only_paths.push(root.path().join("alias"));
        assert!(
            boundary
                .launch_plan(&boundary.workspace, "/bin/true", &[])
                .is_err()
        );
        boundary.read_only_paths.clear();
        let shared = boundary.workspace.join("shared");
        std::fs::write(&shared, "synthetic").unwrap();
        std::fs::hard_link(&shared, root.path().join("shared-alias")).unwrap();
        assert!(
            boundary
                .launch_plan(&boundary.workspace, "/bin/true", &[])
                .is_err()
        );
        std::fs::remove_file(shared).unwrap();
        let fifo =
            std::ffi::CString::new(boundary.workspace.join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            boundary
                .launch_plan(&boundary.workspace, "/bin/true", &[])
                .is_err()
        );
    }

    #[test]
    fn descriptors_are_pinned_cloexec_owned_and_policy_is_sealed() {
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        let plan = plan(&boundary, "/bin/true", &[]);
        let original = plan.inherited[0].metadata().unwrap();
        std::fs::rename(&boundary.workspace, root.path().join("retained")).unwrap();
        std::fs::create_dir(&boundary.workspace).unwrap();
        let pinned = plan.inherited[0].metadata().unwrap();
        assert_eq!(
            (original.dev(), original.ino()),
            (pinned.dev(), pinned.ino())
        );
        assert_ne!(
            pinned.ino(),
            std::fs::metadata(&boundary.workspace).unwrap().ino()
        );
        for file in &plan.inherited {
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC
            );
        }
        let mut policy = sealed_policy().unwrap();
        assert!(policy.write_all(&[0]).is_err());
        // Drop-before-spawn is safe: the command's closure owns the descriptors.
        let (command, _) = plan.into_command();
        drop(command);
    }

    #[test]
    fn malformed_missing_and_conflicting_receipts_fail_closed() {
        for content in [
            "",
            "{}",
            "{\"child-pid\":1}",
            "{\"child-pid\":1}{\"exit-code\":0}",
            "{\"child-pid\":1}{\"exit-code\":1}{}",
            "not json",
        ] {
            let mut status = tempfile::tempfile().unwrap();
            status.write_all(content.as_bytes()).unwrap();
            status.rewind().unwrap();
            assert!(
                ShellLaunchReceipt { status }
                    .verify(ExitStatus::from_raw(256))
                    .is_err()
            );
        }
        let mut status = tempfile::tempfile().unwrap();
        status
            .write_all(b"{\"child-pid\":1}{\"exit-code\":1}")
            .unwrap();
        status.rewind().unwrap();
        assert_eq!(
            ShellLaunchReceipt { status }
                .verify(ExitStatus::from_raw(256))
                .unwrap(),
            1
        );
    }

    #[test]
    #[ignore = "system test: requires bubblewrap 0.11"]
    fn real_setup_failure_is_not_a_child_exit() {
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        // Empty toolchain manifest makes exec impossible even on a fully
        // namespace-capable host. Namespace failure is also setup failure.
        let (output, proof) = run(plan(&boundary, "/missing-program", &[]));
        assert!(!output.status.success());
        assert!(proof.is_err(), "setup failure accepted: {output:?}");
    }

    #[test]
    fn supervisor_helper() {
        if crate::invocation_supervisor_is_requested()
            && let Some(code) = crate::run_invocation_supervisor_if_requested()
        {
            std::process::exit(code);
        }
    }

    #[test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    fn verifier_metadata_remains_read_only_through_workspace_aliases() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let metadata = boundary.workspace.join(".git");
        std::fs::create_dir(&metadata).unwrap();
        std::fs::write(metadata.join("config"), "trusted").unwrap();
        symlink(".git", boundary.workspace.join("alias")).unwrap();
        let plan = boundary.launch_plan_with_protected_paths(
            &boundary.workspace,
            "/bin/sh",
            &["-c".into(), "if echo changed > .git/config; then exit 11; fi; if echo changed > alias/config; then exit 12; fi; if mv .git replaced; then exit 13; fi; echo writable > result".into()],
            std::slice::from_ref(&metadata),
        ).unwrap();
        let (output, receipt) = run(plan);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(receipt.unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(metadata.join("config")).unwrap(),
            "trusted"
        );
        assert_eq!(
            std::fs::read_to_string(boundary.workspace.join("result")).unwrap(),
            "writable\n"
        );
        assert!(
            boundary
                .launch_plan_with_protected_paths(
                    &boundary.workspace,
                    "/bin/true",
                    &[],
                    &[root.path().to_path_buf()],
                )
                .is_err()
        );
    }

    #[test]
    fn nested_protected_metadata_is_rejected_before_launch() {
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        let metadata = boundary.workspace.join("repo/.git");
        std::fs::create_dir_all(&metadata).unwrap();
        let result = boundary.launch_plan_with_protected_paths(
            &boundary.workspace,
            "/bin/true",
            &[],
            &[metadata],
        );
        assert!(
            matches!(result, Err(error) if error.to_string().contains("direct workspace child"))
        );
    }

    #[test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    fn supervised_launch_retains_confinement_and_independent_settlement() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let plan = plan(
            &boundary,
            "/bin/sh",
            &[
                "-c".into(),
                "echo confined > /workspace/result; exit 7".into(),
            ],
        );
        let (mut command, mut owner, receipt) = plan.into_supervised_test_command().unwrap();
        owner.install(&mut command).unwrap();
        let child = command.spawn().unwrap();
        let pid = child.id();
        drop(command);
        owner.started(pid).unwrap();
        let output = child.wait_with_output().unwrap();
        let settlement = owner
            .settle_after_exit_detailed(Some(pid))
            .expect("authoritative settlement");
        assert_eq!(
            settlement.ownership,
            crate::ScopeOwnership::InvocationSupervisor
        );
        assert_eq!(receipt.verify(output.status).unwrap(), 7);
        assert_eq!(
            std::fs::read_to_string(boundary.workspace.join("result")).unwrap(),
            "confined\n"
        );
    }

    #[test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    fn supervised_cancellation_settles_without_a_verifier_result() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let plan = plan(
            &boundary,
            "/bin/sh",
            &[
                "-c".into(),
                "sleep 30 & echo ready > /workspace/ready; wait".into(),
            ],
        );
        let (mut command, mut owner, receipt) = plan.into_supervised_test_command().unwrap();
        owner.install(&mut command).unwrap();
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        drop(command);
        owner.started(pid).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !boundary.workspace.join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "confined command exited before ready"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "confined command did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(owner.request_supervised_termination());
        let output = child.wait_with_output().unwrap();
        let settlement = owner
            .settle_after_exit_detailed(Some(pid))
            .expect("cancelled descendants settled");
        assert_eq!(
            settlement.ownership,
            crate::ScopeOwnership::InvocationSupervisor
        );
        assert!(
            receipt.verify(output.status).is_err(),
            "cancellation must not become a verifier result"
        );
    }

    #[test]
    fn real_spawn_rejects_socket_stdio_before_executing_bwrap() {
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
        let (mut command, _) = plan(&boundary, "/bin/true", &[]).into_command();
        use std::os::fd::OwnedFd;
        command.stdin(Stdio::from(OwnedFd::from(socket)));
        assert_eq!(
            command.spawn().unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
    }

    // System lane: these assertions require unprivileged namespaces and bwrap
    // 0.11. They never convert a setup/security failure into a skipped success.
    #[test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    fn real_restricted_root_confidentiality_environment_and_ipc() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let sentinel = root.path().join("host-sentinel");
        std::fs::write(&sentinel, "synthetic-private").unwrap();
        symlink(&sentinel, boundary.workspace.join("escape")).unwrap();
        let socket = boundary.workspace.join("host.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        // Positive control proves a pathname socket really is reachable before
        // confinement, even though its pathname will be inside the mount.
        std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let script = r#"
import os, socket, errno
assert os.getcwd() == '/workspace'
assert os.environ['HOME'] == '/home/sandbox'
assert os.environ['TMPDIR'] == '/tmp'
assert 'ASTRA_SYNTHETIC_SECRET' not in os.environ
assert not os.path.exists('/workspace/escape')
assert not os.path.exists('/etc/passwd')
assert not os.path.exists('/run')
open('/workspace/result','w').write('ok')
open('/home/sandbox/private','w').write('private')
open('/tmp/private','w').write('private')
assert os.path.exists('/workspace/host.sock')
for family in (socket.AF_UNIX, socket.AF_INET, socket.AF_INET6, socket.AF_NETLINK):
    try: socket.socket(family, socket.SOCK_STREAM)
    except OSError as e: assert e.errno == errno.ENOSYS, e
    else: raise AssertionError('socket syscall admitted')
try: open('/usr/bin/astra-write-probe','w')
except OSError: pass
else: raise AssertionError('toolchain writable')
assert len(os.listdir('/proc/self/fd')) == 4
"#;
        let (mut command, receipt) =
            plan(&boundary, "/usr/bin/python3", &["-c".into(), script.into()]).into_command();
        // Even an added ordinary caller overlay is cleared by bwrap.
        command.env("ASTRA_SYNTHETIC_SECRET", "synthetic");
        let child = command.spawn().unwrap();
        drop(command);
        let output = child.wait_with_output().unwrap();
        let proof = receipt.verify(output.status);
        assert!(
            proof.is_ok(),
            "{proof:?}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(boundary.workspace.join("result")).unwrap(),
            "ok"
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "synthetic-private"
        );
        assert!(!boundary.home.exists());
        assert!(!boundary.temp.exists());
    }

    #[test]
    #[ignore = "system test: requires bubblewrap 0.11 and all mandatory namespaces"]
    fn real_exec_failure_differs_from_exit_one_and_pinned_mount_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        toolchains(&mut boundary);
        let (output, proof) = run(plan(&boundary, "/bin/sh", &["-c".into(), "exit 1".into()]));
        assert_eq!(proof.unwrap_or_else(|e| panic!("{e}; {output:?}")), 1);
        let (output, proof) = run(plan(&boundary, "/not-installed", &[]));
        assert!(!output.status.success());
        assert!(proof.is_err());
        std::fs::write(boundary.workspace.join("identity"), "original").unwrap();
        let pinned = plan(
            &boundary,
            "/bin/sh",
            &["-c".into(), "test \"$(cat identity)\" = original".into()],
        );
        std::fs::rename(&boundary.workspace, root.path().join("retained")).unwrap();
        std::fs::create_dir(&boundary.workspace).unwrap();
        std::fs::write(boundary.workspace.join("identity"), "replacement").unwrap();
        let (output, proof) = run(pinned);
        // bwrap may reject a raced source; it must never run on the replacement.
        if proof.is_ok() {
            assert!(output.status.success(), "{output:?}");
        } else {
            assert!(!output.status.success(), "missing proof on success");
        }
    }
}
