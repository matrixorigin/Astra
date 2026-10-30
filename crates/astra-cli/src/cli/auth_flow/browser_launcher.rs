#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn windows_browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("powershell.exe");
    // The URL is data in the child environment, never shell source. In
    // particular the callback's &state= cannot become another command.
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "try { Start-Process -FilePath $env:ASTRA_LOGIN_URL -ErrorAction Stop; exit 0 } catch { exit 1 }",
        ])
        .env("ASTRA_LOGIN_URL", url);
    command
}

#[cfg(target_os = "linux")]
fn is_wsl_kernel_release(release: &str) -> bool {
    release.to_ascii_lowercase().contains("microsoft")
}

#[cfg(target_os = "linux")]
fn running_in_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| is_wsl_kernel_release(&release))
}

#[cfg(target_os = "linux")]
fn wsl_browser_command(url: &str, wslenv: Option<&std::ffi::OsStr>) -> std::process::Command {
    let mut command = windows_browser_command(url);
    command.env("WSLENV", forwarded_wslenv(wslenv));
    command
}

#[cfg(target_os = "linux")]
fn forwarded_wslenv(existing: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut entries = Vec::new();
    if let Some(existing) = existing {
        for entry in existing.as_bytes().split(|byte| *byte == b':') {
            let name = entry.split(|byte| *byte == b'/').next().unwrap_or_default();
            // WSLENV names are case-sensitive. Replace every exact entry so
            // stale /u, /p or /l flags cannot suppress or transform the URL.
            if !entry.is_empty() && name != b"ASTRA_LOGIN_URL" {
                entries.push(entry);
            }
        }
    }
    entries.push(b"ASTRA_LOGIN_URL/w");
    std::ffi::OsString::from_vec(entries.join(&b':'))
}

use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::Duration;

pub(crate) async fn launch_login_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if running_in_wsl() {
        return run_wsl_launcher(
            wsl_browser_command(&url, std::env::var_os("WSLENV").as_deref()),
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    command.arg(&url);
    #[cfg(target_os = "windows")]
    let command = windows_browser_command(&url);
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    return hand_off_browser(command);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Err("automatic browser opening is unsupported".into())
}

// Native openers may become the browser itself. Preserve that handoff and let
// Tokio collect eventual exits without tying browser lifetime to login lifetime.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn hand_off_browser(command: Command) -> Result<(), String> {
    tokio::process::Command::from(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .map(drop)
        .map_err(|error| format!("browser opener could not start ({:?})", error.kind()))
}

#[cfg(target_os = "linux")]
struct LauncherGroup(Option<u32>);

#[cfg(target_os = "linux")]
impl LauncherGroup {
    fn signal(&mut self) {
        astra_tools::detach::sigkill_process_group_id(self.0.take());
    }

    fn release(&mut self) {
        self.0 = None;
    }
}

#[cfg(target_os = "linux")]
impl Drop for LauncherGroup {
    fn drop(&mut self) {
        self.signal();
    }
}

#[cfg(target_os = "linux")]
async fn stop_launcher(
    child: &mut tokio::process::Child,
    group: &mut LauncherGroup,
    timeout: Duration,
) -> bool {
    group.signal();
    let _ = child.start_kill();
    matches!(tokio::time::timeout(timeout, child.wait()).await, Ok(Ok(_)))
}

#[cfg(target_os = "linux")]
async fn run_wsl_launcher(
    command: Command,
    attempt_timeout: Duration,
    cleanup_timeout: Duration,
) -> Result<(), String> {
    let child = tokio::process::Command::from(command)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("powershell.exe could not start ({:?})", error.kind()))?;
    // wait() clears Child::id(), but a failed wrapper can leave group members.
    // Tuple order also signals the group before dropping the child if the
    // owner is destroyed without being polled, including runtime shutdown.
    let resources = (LauncherGroup(child.id()), child);
    let (mut sender, receiver) = tokio::sync::oneshot::channel();
    // This task owns only process collection. Dropping the login's receiver
    // requests local cleanup; it cannot publish progress or log in. POSIX group
    // signaling cannot revoke a Windows browser request already in flight.
    let owner = tokio::spawn(async move {
        let mut resources = resources;
        let (group, child) = &mut resources;
        let outcome = tokio::select! {
            biased;
            _ = sender.closed() => {
                let _ = stop_launcher(child, group, cleanup_timeout).await;
                return;
            }
            outcome = tokio::time::timeout(attempt_timeout, child.wait()) => outcome,
        };
        let reason = match outcome {
            Ok(Ok(status)) if status.success() => {
                // Successful handoff may leave a browser alive. Do not kill it.
                group.release();
                let _ = sender.send(Ok(()));
                return;
            }
            Ok(Ok(status)) => {
                use std::os::unix::process::ExitStatusExt;
                format!(
                    "exited unsuccessfully (code {:?}, signal {:?})",
                    status.code(),
                    status.signal()
                )
            }
            Ok(Err(error)) => format!("wait failed ({:?})", error.kind()),
            Err(_) => "timed out".to_string(),
        };
        let collected = stop_launcher(child, group, cleanup_timeout).await;
        let _ = sender.send(Err(format!(
            "powershell.exe {reason}{}",
            if collected {
                ""
            } else {
                "; local launcher collection is unverified"
            }
        )));
    });
    let result = receiver.await;
    match (result, owner.await) {
        (Ok(result), Ok(())) => result,
        _ => Err("powershell.exe process owner failed; local cleanup is unverified".into()),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn wsl_login_uses_windows_browser_without_interpreting_callback_url() {
        assert!(super::is_wsl_kernel_release(
            "5.15.167.4-microsoft-standard-WSL2"
        ));
        assert!(super::is_wsl_kernel_release("4.4.0-Microsoft"));
        assert!(!super::is_wsl_kernel_release("6.8.0-generic"));
        let url =
            "https://uc.example.com/authorize?client_id=astra-cli&response_type=code&state=abc";
        let wsl = super::wsl_browser_command(url, None);
        assert_eq!(wsl.get_program(), std::ffi::OsStr::new("powershell.exe"));
        assert!(wsl.get_envs().any(
            |(key, value)| key == "ASTRA_LOGIN_URL" && value == Some(std::ffi::OsStr::new(url))
        ));
        assert!(wsl.get_envs().any(|(key, value)| {
            key == "WSLENV" && value == Some(std::ffi::OsStr::new("ASTRA_LOGIN_URL/w"))
        }));
        assert!(
            wsl.get_args()
                .all(|arg| !arg.to_string_lossy().contains(url))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wsl_login_normalizes_forwarded_environment() {
        for (existing, expected) in [
            (
                "WT_SESSION:GTK_MODULES/w",
                "WT_SESSION:GTK_MODULES/w:ASTRA_LOGIN_URL/w",
            ),
            (
                "WT_SESSION:astra_login_url/p",
                "WT_SESSION:astra_login_url/p:ASTRA_LOGIN_URL/w",
            ),
            ("ASTRA_LOGIN_URL", "ASTRA_LOGIN_URL/w"),
            ("ASTRA_LOGIN_URL/u", "ASTRA_LOGIN_URL/w"),
            ("ASTRA_LOGIN_URL/p", "ASTRA_LOGIN_URL/w"),
            ("ASTRA_LOGIN_URL/l", "ASTRA_LOGIN_URL/w"),
            (
                "ASTRA_LOGIN_URL/u:FOO:ASTRA_LOGIN_URL/p",
                "FOO:ASTRA_LOGIN_URL/w",
            ),
            ("FOO:ASTRA_LOGIN_URL/w", "FOO:ASTRA_LOGIN_URL/w"),
        ] {
            let command = super::wsl_browser_command(
                "https://uc.example.com/authorize",
                Some(std::ffi::OsStr::new(existing)),
            );
            assert!(command.get_envs().any(|(key, value)| {
                key == "WSLENV" && value == Some(std::ffi::OsStr::new(expected))
            }));
        }
    }

    #[test]
    fn windows_browser_launch_keeps_callback_url_out_of_shell_source() {
        let url = "https://thememoria.ai/connect/astra?port=1234&state=abc&cli_version=0.2.1";
        let command = super::windows_browser_command(url);
        let args: Vec<_> = command
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .all(|arg| !arg.contains(url) && !arg.contains("&state"))
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "ASTRA_LOGIN_URL"
                    && value == Some(std::ffi::OsStr::new(url)))
        );
    }

    #[cfg(target_os = "linux")]
    mod processes {
        use super::super::{hand_off_browser, run_wsl_launcher, wsl_browser_command};
        use nix::{
            errno::Errno,
            sys::{
                signal::{Signal, kill},
                wait::{WaitPidFlag, waitpid},
            },
            unistd::Pid,
        };
        use std::path::Path;
        use std::process::Command;
        use std::time::Duration;

        fn shell(script: &str, paths: &[&Path]) -> Command {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", script, "browser-test"]).args(paths);
            command
        }

        fn read_pid(path: &Path) -> Option<Pid> {
            std::fs::read_to_string(path)
                .ok()?
                .parse::<i32>()
                .ok()
                .map(Pid::from_raw)
        }

        async fn pid_file(path: &Path) -> Pid {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(pid) = read_pid(path) {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("launcher readiness")
        }

        async fn collected(pid: Pid) {
            tokio::time::timeout(Duration::from_secs(3), async {
                while kill(pid, None).is_ok() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("launcher must terminate and be collected");
            assert_eq!(kill(pid, None), Err(Errno::ESRCH));
            assert_eq!(waitpid(pid, Some(WaitPidFlag::WNOHANG)), Err(Errno::ECHILD));
        }

        #[tokio::test]
        async fn launcher_observes_completion_and_sanitizes_failure() {
            let dir = tempfile::tempdir().unwrap();
            run_wsl_launcher(
                shell("exit 0", &[]),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            for (script, expected) in [
                ("exit 7", "code Some(7)"),
                ("kill -TERM $$", "signal Some(15)"),
            ] {
                let error = run_wsl_launcher(
                    shell(script, &[]),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                )
                .await
                .unwrap_err();
                assert!(error.contains(expected), "{error}");
            }
            let error = run_wsl_launcher(
                Command::new(dir.path().join("missing")),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
            assert!(error.contains("could not start (NotFound)"));

            let secret = "synthetic-URL-state-that-must-not-appear";
            let mut command = shell("printf '%s' \"$SECRET\" >&2; exit 7", &[]);
            command.env("SECRET", secret);
            let error = run_wsl_launcher(command, Duration::from_secs(1), Duration::from_secs(1))
                .await
                .unwrap_err();
            assert!(error.contains("powershell.exe") && error.contains("Some(7)"));
            assert!(!error.contains(secret));
        }

        #[tokio::test]
        async fn wsl_dispatch_does_not_invoke_an_alternate_launcher() {
            if let Ok(expected) = std::env::var("ASTRA_WSL_LAUNCH_EXPECTED") {
                let error = super::super::launch_login_url(
                    "https://login.example/authorize?state=synthetic".into(),
                )
                .await
                .unwrap_err();
                assert!(error.contains(&expected), "{error}");
                return;
            }
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let fallback = dir.path().join("wslview");
            let marker = dir.path().join("fallback-invoked");
            std::fs::write(
                &fallback,
                "#!/bin/sh\nprintf invoked > \"$FALLBACK_MARKER\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();
            let primary = dir.path().join("powershell.exe");
            for (script, expected) in [
                (None, "could not start (NotFound)"),
                (Some("exit 7"), "code Some(7)"),
                (Some("exec /bin/sleep 30"), "timed out"),
            ] {
                if let Some(script) = script {
                    std::fs::write(&primary, format!("#!/bin/sh\n{script}\n")).unwrap();
                    std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let test = concat!(
                    module_path!(),
                    "::wsl_dispatch_does_not_invoke_an_alternate_launcher"
                )
                .split_once("::")
                .unwrap()
                .1;
                let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", test, "--nocapture"])
                    .env("ASTRA_WSL_LAUNCH_EXPECTED", expected)
                    .env("WSL_DISTRO_NAME", "Astra-test")
                    .env("PATH", dir.path())
                    .env("FALLBACK_MARKER", &marker)
                    .kill_on_drop(true);
                let output = tokio::time::timeout(Duration::from_secs(12), child.output())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
                assert!(!marker.exists(), "{expected}");
            }
        }

        struct GroupGuard(Pid);

        impl Drop for GroupGuard {
            fn drop(&mut self) {
                let _ = nix::sys::signal::killpg(self.0, Signal::SIGKILL);
            }
        }

        async fn collect_group(group: Pid) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match waitpid(Pid::from_raw(-group.as_raw()), Some(WaitPidFlag::WNOHANG)) {
                        Err(Errno::ECHILD) => break,
                        Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Ok(_) => {}
                        Err(error) => panic!("collect launcher group: {error}"),
                    }
                }
            })
            .await
            .expect("launcher descendants must settle");
        }

        fn delegated_command(dir: &Path, mode: &str) -> Command {
            let mut command = shell(
                r#"
                    printf '%s' "$$" > "$PARENT"
                    /bin/sh -c 'printf "%s" "$$" > "$CHILD"; while [ ! -e "$GATE" ]; do /bin/sleep 0.01; done; printf launched > "$EFFECT"' &
                    case "$MODE" in
                        exit|success)
                            while [ ! -s "$CHILD" ]; do /bin/sleep 0.01; done
                            [ "$MODE" = success ] && exit 0
                            exit 7 ;;
                        *) wait ;;
                    esac
                "#,
                &[],
            );
            command
                .env("PARENT", dir.join("parent"))
                .env("CHILD", dir.join("child"))
                .env("GATE", dir.join("gate"))
                .env("EFFECT", dir.join("effect"))
                .env("MODE", mode);
            command
        }

        async fn delegated_launcher_case(mode: &str) {
            // Adopt this fixture's orphaned children so the test proves their
            // termination and reaps them without depending on the host's init.
            nix::sys::prctl::set_child_subreaper(true).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let parent_path = dir.path().join("parent");
            let child_path = dir.path().join("child");
            let gate = dir.path().join("gate");
            let effect = dir.path().join("effect");
            let command = delegated_command(dir.path(), mode);
            let task = tokio::spawn(run_wsl_launcher(
                command,
                Duration::from_secs(if mode == "timeout" { 1 } else { 5 }),
                Duration::from_secs(1),
            ));
            let parent = pid_file(&parent_path).await;
            let _guard = GroupGuard(parent);
            let child = pid_file(&child_path).await;
            if mode == "cancel" {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                let result = tokio::time::timeout(Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap();
                match mode {
                    "success" => result.unwrap(),
                    "exit" => assert!(result.unwrap_err().contains("code Some(7)")),
                    "timeout" => assert!(result.unwrap_err().contains("timed out")),
                    _ => unreachable!(),
                }
            }
            collected(parent).await;
            if mode == "success" {
                assert!(kill(child, None).is_ok(), "successful handoff must survive");
            }
            // Releasing the effect only after the parent has been collected
            // catches cleanup that kills a wrapper but leaves its work alive.
            std::fs::write(&gate, "continue").unwrap();
            collect_group(parent).await;
            assert_eq!(effect.exists(), mode == "success", "{mode}");
            assert_eq!(kill(child, None), Err(Errno::ESRCH));
        }

        #[test]
        fn runtime_shutdown_signals_polled_and_unpolled_launcher_groups() {
            use std::future::{Future, poll_fn};
            use std::task::Poll;
            use std::time::Instant;

            nix::sys::prctl::set_child_subreaper(true).unwrap();
            for polled in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut launch = Box::pin(run_wsl_launcher(
                    delegated_command(dir.path(), "wait"),
                    Duration::from_secs(30),
                    Duration::from_secs(1),
                ));
                // Spawn the owner without yielding to it: its captured resource
                // tuple must clean up even if the async body never runs.
                runtime.block_on(poll_fn(|context| {
                    assert!(launch.as_mut().poll(context).is_pending());
                    Poll::Ready(())
                }));
                if polled {
                    runtime.block_on(async {
                        assert!(
                            tokio::time::timeout(Duration::from_millis(20), launch.as_mut())
                                .await
                                .is_err()
                        );
                    });
                }
                // Readiness is synchronous so the queued owner stays unpolled.
                let deadline = Instant::now() + Duration::from_secs(3);
                let (parent, child) = loop {
                    if let Some(parent) = read_pid(&dir.path().join("parent"))
                        && let Some(child) = read_pid(&dir.path().join("child"))
                    {
                        break (parent, child);
                    }
                    assert!(Instant::now() < deadline, "launcher readiness");
                    std::thread::sleep(Duration::from_millis(5));
                };
                let _guard = GroupGuard(parent);
                drop(runtime);
                drop(launch);
                std::fs::write(dir.path().join("gate"), "continue").unwrap();
                // The destroyed runtime cannot await reaping; the independent
                // fixture collector settles every process before asserting.
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(collect_group(parent));
                assert!(!dir.path().join("effect").exists(), "owner polled={polled}");
                assert_eq!(kill(child, None), Err(Errno::ESRCH));
            }
        }

        #[tokio::test]
        async fn failed_or_cancelled_launcher_stops_delegated_local_work() {
            for mode in ["timeout", "cancel", "exit"] {
                delegated_launcher_case(mode).await;
            }
        }

        #[tokio::test]
        async fn successful_wsl_handoff_preserves_the_browser() {
            delegated_launcher_case("success").await;
        }

        #[tokio::test]
        async fn native_handoff_does_not_kill_a_long_lived_browser() {
            let dir = tempfile::tempdir().unwrap();
            let pid_path = dir.path().join("pid");
            hand_off_browser(shell(
                "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 30",
                &[&pid_path],
            ))
            .unwrap();
            let pid = pid_file(&pid_path).await;
            assert!(kill(pid, None).is_ok());
            kill(pid, Signal::SIGKILL).unwrap();
            collected(pid).await;
        }

        #[tokio::test]
        async fn concurrent_launches_keep_url_and_wslenv_in_child_data() {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let stub = dir.path().join("powershell.exe");
            std::fs::write(
                &stub,
                "#!/bin/sh\nprintf '%s\\n%s' \"$ASTRA_LOGIN_URL\" \"$WSLENV\" > \"$OUT_FILE\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
            let parent_url = std::env::var_os("ASTRA_LOGIN_URL");
            let parent_wslenv = std::env::var_os("WSLENV");
            let mut tasks = tokio::task::JoinSet::new();
            for index in 0..8 {
                let path = dir.path().to_owned();
                tasks.spawn(async move {
                    let url = format!("https://login.example/authorize?state={index}&data=';$()/");
                    let output = path.join(format!("received-{index}"));
                    let mut primary = wsl_browser_command(
                        &url,
                        Some(std::ffi::OsStr::new("KEEP/p:ASTRA_LOGIN_URL/u")),
                    );
                    primary.env("PATH", &path).env("OUT_FILE", &output);
                    run_wsl_launcher(primary, Duration::from_secs(2), Duration::from_secs(1))
                        .await
                        .unwrap();
                    assert_eq!(
                        std::fs::read_to_string(output).unwrap(),
                        format!("{url}\nKEEP/p:ASTRA_LOGIN_URL/w")
                    );
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
            assert_eq!(std::env::var_os("ASTRA_LOGIN_URL"), parent_url);
            assert_eq!(std::env::var_os("WSLENV"), parent_wslenv);
        }

        #[test]
        fn wslenv_preserves_non_utf8_bytes_and_is_idempotent() {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let input = std::ffi::OsString::from_vec(
                b"KEEP\xff/p:ASTRA_LOGIN_URL/u:ASTRA_LOGIN_URL/l".to_vec(),
            );
            let output = super::super::forwarded_wslenv(Some(&input));
            assert_eq!(output.as_bytes(), b"KEEP\xff/p:ASTRA_LOGIN_URL/w");
            assert_eq!(super::super::forwarded_wslenv(Some(&output)), output);
        }
    }
}
