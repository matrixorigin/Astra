//! Bounded synchronous subprocess execution with the shared invocation owner.
//! Callers retain responsibility for admission and filesystem/network isolation.
use crate::{BashInvocationOwner, ScopeOwnership};
use std::io::{self, Read};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SyncProcessError {
    pub phase: &'static str,
    pub detail: String,
    pub started: bool,
    pub ownership: Option<ScopeOwnership>,
}
impl std::fmt::Display for SyncProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failed ({}): {}",
            self.phase,
            if self.started {
                "after launch; effects may have occurred"
            } else {
                "no command was run"
            },
            self.detail
        )
    }
}
impl std::error::Error for SyncProcessError {}

#[derive(Debug)]
pub struct SyncProcessOutput {
    pub output: Output,
    pub ownership: Option<ScopeOwnership>,
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn terminate(child: &mut Child, owner: &mut BashInvocationOwner) -> Option<ScopeOwnership> {
    let pid = child.id();
    if owner.is_supervised() {
        let _ = owner.request_supervised_termination();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return owner.settle_after_exit(Some(pid)),
                Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        kill(child);
        let _ = child.wait();
        return None;
    }
    kill(child);
    let _ = child.wait();
    owner.settle_after_exit(Some(pid))
}

#[cfg(unix)]
fn prepare_pipe(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(windows)]
fn prepare_pipe(_: &impl std::os::windows::io::AsRawHandle) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn read_available(pipe: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    pipe.read(buf)
}
#[cfg(windows)]
fn read_available(
    pipe: &mut (impl Read + std::os::windows::io::AsRawHandle),
    buf: &mut [u8],
) -> io::Result<usize> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            handle: *mut std::ffi::c_void,
            buffer: *mut std::ffi::c_void,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            remaining: *mut u32,
        ) -> i32;
    }
    let mut available = 0;
    let ok = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(109) {
            return Ok(0);
        }
        return Err(error);
    }
    if available == 0 {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    let count = buf.len().min(available as usize);
    pipe.read(&mut buf[..count])
}

// Use concrete child pipe types to share Unix and Windows descriptor bounds.
macro_rules! drain {
    ($pipe:expr, $buffer:expr, $total:expr, $limit:expr) => {{
        let mut eof = false;
        let mut bytes = [0; 8192];
        // Fairness budget: output flooding cannot starve timeout/other pipe.
        for _ in 0..8 {
            match read_available($pipe, &mut bytes) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    if $total.saturating_add(n) > $limit {
                        return Err((
                            "output limit",
                            format!(
                                "output exceeded {} bytes; incomplete output was discarded",
                                $limit
                            ),
                        ));
                    }
                    $total += n;
                    $buffer.extend_from_slice(&bytes[..n]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(("output read", error.to_string())),
            }
        }
        eof
    }};
}

/// Execute a program without a shell. Uses the actual command returned by the
/// shared owner (including its supervisor wrapper). No raw spawn escapes this API.
pub fn run_sync_process(
    program: &str,
    args: &[String],
    timeout: Duration,
    output_limit: usize,
    configure: impl FnOnce(&mut Command) -> io::Result<()>,
) -> Result<SyncProcessOutput, SyncProcessError> {
    let before = |phase, error: io::Error| SyncProcessError {
        phase,
        detail: error.to_string(),
        started: false,
        ownership: None,
    };
    let (mut command, mut owner) =
        BashInvocationOwner::prepare(program, args).map_err(|e| before("owner setup", e))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    configure(&mut command).map_err(|e| before("command setup", e))?;
    owner
        .install(&mut command)
        .map_err(|e| before("owner setup", e))?;
    let mut child = command.spawn().map_err(|e| before("spawn", e))?;
    let pid = child.id();
    let result = (|| -> Result<Output, (&'static str, String)> {
        owner
            .started(pid)
            .map_err(|e| ("owner handshake", e.to_string()))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        prepare_pipe(&stdout)
            .and_then(|()| prepare_pipe(&stderr))
            .map_err(|e| ("pipe setup", e.to_string()))?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut total: usize = 0;
        let deadline = Instant::now() + timeout;
        let mut status = None;
        let mut exit_deadline = None;
        loop {
            let out_eof = drain!(&mut stdout, out, total, output_limit);
            let err_eof = drain!(&mut stderr, err, total, output_limit);
            if status.is_none() {
                status = child.try_wait().map_err(|e| ("wait", e.to_string()))?;
                if status.is_some() {
                    exit_deadline = Some(Instant::now() + Duration::from_millis(250));
                }
            }
            if let Some(status) = status {
                if out_eof && err_eof {
                    return Ok(Output {
                        status,
                        stdout: out,
                        stderr: err,
                    });
                }
                if exit_deadline.is_some_and(|end| Instant::now() >= end) {
                    return Err(("output completion", "descendants retained output pipes after leader exit; incomplete output was discarded".to_string()));
                }
            }
            if Instant::now() >= deadline {
                return Err(("timeout", format!("deadline of {timeout:?} exceeded")));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    match result {
        Ok(output) => Ok(SyncProcessOutput {
            output,
            ownership: owner.settle_after_exit(Some(pid)),
        }),
        Err((phase, detail)) => {
            let ownership = terminate(&mut child, &mut owner);
            Err(SyncProcessError {
                phase,
                detail,
                started: true,
                ownership,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn bounded_output_and_descendant_pipes_do_not_hang() {
        let run = |script: &str, limit| {
            run_sync_process(
                "sh",
                &["-c".into(), script.into()],
                Duration::from_millis(100),
                limit,
                |_| Ok(()),
            )
        };
        let output = run("printf ok; printf err >&2; exit 7", 1024).unwrap();
        assert_eq!(output.output.status.code(), Some(7));
        assert_eq!(output.output.stdout, b"ok");
        assert_eq!(output.output.stderr, b"err");
        assert_eq!(
            run("while :; do printf 1234567890; done", 32)
                .unwrap_err()
                .phase,
            "output limit"
        );
        let start = Instant::now();
        assert_eq!(run("sleep 30 & wait", 1024).unwrap_err().phase, "timeout");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[cfg(unix)]
    #[test]
    fn output_completion_deadline_stops_inherited_pipe_writer() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late-write");
        let start = Instant::now();
        let error = run_sync_process(
            "sh",
            &[
                "-c".into(),
                "(sleep 1; printf leaked > late-write) & exit 0".into(),
            ],
            Duration::from_secs(5),
            1024,
            |command| {
                command.current_dir(dir.path());
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.phase, "output completion");
        assert!(error.started);
        assert!(start.elapsed() >= Duration::from_millis(250));
        assert!(start.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            !marker.exists(),
            "descendant must not perform its delayed write after cleanup"
        );
    }
}
