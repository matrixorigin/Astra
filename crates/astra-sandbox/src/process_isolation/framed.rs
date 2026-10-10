//! Bounded newline-delimited process I/O on the existing invocation owner.
//! Admission, JSON interpretation, native sessions and requests belong to the
//! selected CLI/User Runner adapter, not this physical process boundary.

use super::{
    BashInvocationOwner, InvocationSupervisor, IsolatedScopeAbortGuard, OUTPUT_DRAIN_TIMEOUT,
    READ_CHUNK_SIZE, ScopeSettlement, apply_process_scope, settle_isolated_owner_after_exit,
    terminate_isolated_child,
};
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Explicit resource budget for one physical process, not a logical session.
#[derive(Debug, Clone, Copy)]
pub struct FramedProcessLimits {
    /// Payload bytes per stdin/stdout frame, excluding the LF delimiter.
    pub max_frame_bytes: usize,
    /// Capacity of each input/output queue. Slow consumers apply backpressure.
    pub max_queued_frames: usize,
    /// Retained stderr prefix; excess diagnostics are drained, not accumulated.
    pub max_stderr_bytes: usize,
    /// Physical invocation deadline, including its ownership handshake.
    pub timeout: Duration,
}

impl FramedProcessLimits {
    fn deadline(self) -> io::Result<Instant> {
        // Include both queues, their active frames and Vec growth. This is a
        // retained-buffer budget, not a lifetime quota on a long-running stream
        // or a claim about total RSS (queue metadata/read chunks are additional).
        let buffered = self
            .max_queued_frames
            .checked_add(2)
            .and_then(|slots| slots.checked_mul(self.max_frame_bytes.max(8)))
            .and_then(|bytes| bytes.checked_mul(4))
            .and_then(|bytes| {
                self.max_stderr_bytes
                    .max(8)
                    .checked_mul(2)
                    .and_then(|stderr| bytes.checked_add(stderr))
            });
        if self.max_frame_bytes == 0
            || self.max_queued_frames == 0
            || self.max_queued_frames > 1024
            || self.timeout.is_zero()
            || buffered.is_none_or(|bytes| bytes > 64 * 1024 * 1024)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid framed process budget",
            ));
        }
        Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "process deadline overflow"))
    }
}

/// Cloneable input endpoint. A successful send means queued, not written or
/// accepted by the native protocol. Native request/response IDs provide that
/// acknowledgement. No caller-owned payload is copied until a slot is reserved.
#[derive(Debug, Clone)]
pub struct FramedProcessInput {
    sender: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    max_frame_bytes: usize,
}

impl FramedProcessInput {
    pub async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
        if frame.is_empty()
            || frame.len() > self.max_frame_bytes
            || frame.iter().any(|byte| matches!(*byte, b'\n' | b'\r'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid input frame",
            ));
        }
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "process cancelled")),
            permit = self.sender.reserve() => {
                let permit = permit.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process input closed"))?;
                permit.send(frame.to_vec());
                Ok(())
            }
        }
    }
}

/// Physical transport stop cause; none of these implies a native turn outcome.
#[derive(Debug)]
pub enum FramedProcessEnd {
    Exited,
    Cancelled,
    TimedOut,
    OwnershipFailed(io::Error),
    WaitFailed(io::Error),
    InputFailed(io::Error),
    OutputFailed(io::Error),
    StderrFailed(io::Error),
    OutputDrainTimedOut,
}

/// Returned after a bounded cleanup attempt. Only an authoritative settlement
/// proves descendant cleanup, even when the exit code is zero. stderr is
/// diagnostic bytes, not model input.
#[derive(Debug)]
pub struct FramedProcessOutcome {
    pub end: FramedProcessEnd,
    /// Whether authenticated START was sent. This permits target execution,
    /// not a native request acknowledgement. None means ownership evidence
    /// was lost (e.g. a handshake worker panic), never "not started".
    pub target_released: Option<bool>,
    pub status: Option<ExitStatus>,
    pub settlement: Option<ScopeSettlement>,
    pub stderr: Vec<u8>,
    pub stderr_capped: bool,
}

/// I/O handle for exactly one invocation owned by `BashInvocationOwner`.
/// Dropping it requests cancellation; use `cancel_and_wait` to obtain settlement
/// evidence. While Tokio remains alive, the private driver continues cleanup if
/// a caller future is dropped; runtime teardown retains the owner's RAII guard.
pub struct FramedProcess {
    input: Option<FramedProcessInput>,
    output: mpsc::Receiver<Vec<u8>>,
    completion: Option<JoinHandle<FramedProcessOutcome>>,
    cancel: CancellationToken,
}

impl FramedProcess {
    pub fn input(&self) -> FramedProcessInput {
        self.input.as_ref().expect("live process input").clone()
    }

    /// Complete LF-delimited stdout payloads only. EOF or a closed queue is not
    /// a native success: inspect the final outcome and the adapter's protocol.
    pub async fn recv_frame(&mut self) -> Option<Vec<u8>> {
        self.output.recv().await
    }

    /// Cancels this invocation, never the caller's parent cancellation token.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Close this handle's input and drain/discard remaining frames while
    /// waiting. Retained input clones must also be dropped for stdin EOF.
    /// Consumers that need all evidence must receive it before calling wait.
    pub async fn wait(mut self) -> Result<FramedProcessOutcome, tokio::task::JoinError> {
        self.input.take();
        while self.output.recv().await.is_some() {}
        self.completion.take().expect("process completion").await
    }

    pub async fn cancel_and_wait(self) -> Result<FramedProcessOutcome, tokio::task::JoinError> {
        self.cancel();
        self.wait().await
    }
}

impl Drop for FramedProcess {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl BashInvocationOwner {
    /// Prepare a start-gated command for structured transport. A cgroup alone
    /// owns descendants but cannot defer target exec until cancellation and
    /// deadline admission. Retain the existing supervisor even with a cgroup.
    pub fn prepare_framed(
        target_program: &str,
        target_args: &[String],
    ) -> io::Result<(Command, Self)> {
        let process_scope = apply_process_scope();
        let (command, supervisor) = InvocationSupervisor::prepare(target_program, target_args)?;
        Ok((
            command,
            Self {
                process_scope,
                supervisor: Some(supervisor),
            },
        ))
    }

    /// Spawn a bounded bidirectional LF-framed transport using this owner.
    ///
    /// Supply the command returned by `prepare_framed` after applying the selected
    /// executor's environment, cwd and sandbox policy. This method installs
    /// ownership last and requires supervisor start-gating before spawning;
    /// it does not grant filesystem, credential or network authority. A Tokio
    /// runtime is required. Handshake failures after spawn appear in the final
    /// outcome, with no fabricated settlement receipt.
    pub fn spawn_framed(
        self,
        mut command: Command,
        limits: FramedProcessLimits,
        cancel: CancellationToken,
    ) -> io::Result<FramedProcess> {
        let deadline = limits.deadline()?;
        tokio::runtime::Handle::try_current()
            .map_err(|error| io::Error::other(format!("framed process requires Tokio: {error}")))?;
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "process cancelled before spawn",
            ));
        }
        if !self.is_supervised() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "framed process requires a start-gated invocation supervisor",
            ));
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        self.install(&mut command)?;
        let supervised = self.is_supervised();
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(!supervised);
        let child = command.spawn()?;
        let guard = IsolatedScopeAbortGuard::new(Some(self), None, child.id());
        let cancel = cancel.child_token();
        let (input_tx, input_rx) = mpsc::channel(limits.max_queued_frames);
        let (output_tx, output_rx) = mpsc::channel(limits.max_queued_frames);
        let input = FramedProcessInput {
            sender: input_tx,
            cancel: cancel.clone(),
            max_frame_bytes: limits.max_frame_bytes,
        };
        let completion = tokio::spawn(drive(
            child,
            guard,
            input_rx,
            output_tx,
            limits,
            deadline,
            cancel.clone(),
        ));
        Ok(FramedProcess {
            input: Some(input),
            output: output_rx,
            completion: Some(completion),
            cancel,
        })
    }
}

async fn read_frames(
    mut reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<Vec<u8>>,
    limit: usize,
) -> io::Result<()> {
    let mut buffer = [0u8; READ_CHUNK_SIZE];
    let mut frame = Vec::new();
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return if frame.is_empty() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated stdout frame",
                ))
            };
        }
        for part in buffer[..count].split_inclusive(|byte| *byte == b'\n') {
            let complete = part.last() == Some(&b'\n');
            let payload = if complete {
                &part[..part.len() - 1]
            } else {
                part
            };
            if payload.len() > limit.saturating_sub(frame.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stdout frame exceeded byte limit",
                ));
            }
            frame.extend_from_slice(payload);
            if complete {
                sender.send(std::mem::take(&mut frame)).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "stdout consumer closed")
                })?;
            }
        }
    }
}

async fn write_frames(
    mut writer: impl AsyncWrite + Unpin,
    mut receiver: mpsc::Receiver<Vec<u8>>,
) -> io::Result<()> {
    while let Some(frame) = receiver.recv().await {
        writer.write_all(&frame).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    writer.shutdown().await
}

async fn drain_stderr(
    mut reader: impl AsyncRead + Unpin,
    retained: &mut Vec<u8>,
    capped: &mut bool,
    limit: usize,
) -> io::Result<()> {
    let mut buffer = [0u8; READ_CHUNK_SIZE];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let keep = count.min(limit.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..keep]);
        *capped |= keep < count;
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn drive(
    mut child: tokio::process::Child,
    guard: IsolatedScopeAbortGuard,
    input: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<Vec<u8>>,
    limits: FramedProcessLimits,
    deadline: Instant,
    cancel: CancellationToken,
) -> FramedProcessOutcome {
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let leader_pid = child.id();
    let supervised = guard
        .owner
        .as_ref()
        .expect("invocation owner")
        .is_supervised();
    let start_cancel = cancel.clone();
    let started = tokio::task::spawn_blocking(move || {
        // Transfer the whole abort guard, not just its owner. If the driver
        // disappears during this await, its cancel guard closes the start
        // gate and the blocking worker still owns supervisor cleanup.
        let mut guard = guard;
        let check_start = || {
            if start_cancel.is_cancelled() {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "process cancelled before START",
                ))
            } else if Instant::now() >= deadline {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process deadline exceeded before START",
                ))
            } else {
                Ok(())
            }
        };
        let result = leader_pid
            .ok_or_else(|| io::Error::other("child PID unavailable"))
            .and_then(|pid| {
                guard
                    .owner
                    .as_mut()
                    .expect("invocation owner")
                    .started_before(pid, deadline.into_std(), &check_start)
            });
        (guard, result)
    })
    .await;
    let (mut guard, startup_error) = match started {
        Ok((guard, result)) => (guard, result.err()),
        Err(error) => {
            let mut guard = IsolatedScopeAbortGuard::new(None, None, leader_pid);
            guard.disarm(); // The failed worker's real guard owns cleanup.
            (
                guard,
                Some(io::Error::other(format!(
                    "ownership handshake worker failed: {error}"
                ))),
            )
        }
    };
    let mut stderr = Vec::new();
    let mut stderr_capped = false;
    let mut status = None;
    let mut settlement = None;
    let target_released = guard
        .owner
        .as_ref()
        .and_then(|owner| owner.supervisor.as_ref())
        .and_then(super::InvocationSupervisor::target_released);
    let end = if let Some(error) = startup_error {
        match error.kind() {
            io::ErrorKind::Interrupted => FramedProcessEnd::Cancelled,
            io::ErrorKind::TimedOut => FramedProcessEnd::TimedOut,
            _ => FramedProcessEnd::OwnershipFailed(error),
        }
    } else {
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdout = read_frames(stdout, output, limits.max_frame_bytes);
        let stdin = write_frames(stdin, input);
        let stderr_reader = drain_stderr(
            stderr_pipe,
            &mut stderr,
            &mut stderr_capped,
            limits.max_stderr_bytes,
        );
        tokio::pin!(stdout, stdin, stderr_reader);
        let (mut stdout_done, mut stdin_done, mut stderr_done) = (false, false, false);
        let mut drain_deadline = None;
        loop {
            if status.is_some() && stdout_done && stderr_done {
                break FramedProcessEnd::Exited;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break FramedProcessEnd::Cancelled,
                _ = tokio::time::sleep_until(deadline) => break FramedProcessEnd::TimedOut,
                _ = async { tokio::time::sleep_until(drain_deadline.unwrap_or(deadline)).await }, if drain_deadline.is_some() => {
                    break FramedProcessEnd::OutputDrainTimedOut;
                }
                result = &mut stdout, if !stdout_done => {
                    match result { Ok(()) => stdout_done = true, Err(error) => break FramedProcessEnd::OutputFailed(error) }
                }
                result = &mut stdin, if !stdin_done => {
                    match result { Ok(()) => stdin_done = true, Err(error) => break FramedProcessEnd::InputFailed(error) }
                }
                result = &mut stderr_reader, if !stderr_done => {
                    match result { Ok(()) => stderr_done = true, Err(error) => break FramedProcessEnd::StderrFailed(error) }
                }
                result = child.wait(), if status.is_none() => {
                    match result {
                        Ok(exit) => status = Some(exit),
                        Err(error) => break FramedProcessEnd::WaitFailed(error),
                    }
                    settlement = settle_isolated_owner_after_exit(guard.take_owner().expect("started owner"), leader_pid).await;
                    guard.disarm();
                    drain_deadline = Some(Instant::now() + OUTPUT_DRAIN_TIMEOUT);
                }
            }
        }
    };
    // The I/O futures (including blocked reads, writes and queue sends) have
    // now dropped. Never kill a supervisor before requesting its settlement.
    if status.is_none() {
        if guard.owner.is_some() {
            settlement =
                terminate_isolated_child(&mut child, guard.take_owner(), None, leader_pid).await;
        } else {
            // A failed blocking worker dropped its owner/control endpoint.
            // Give the subreaper time to finish; leader-only SIGKILL first
            // could release its adopted descendants. No receipt survives this
            // failure, regardless of whether the helper exits successfully.
            if !supervised
                || tokio::time::timeout(Duration::from_secs(3), child.wait())
                    .await
                    .is_err()
            {
                let _ = child.kill().await;
            }
            let _ = child.wait().await;
        }
        status = child.try_wait().ok().flatten();
        guard.disarm();
    }
    FramedProcessOutcome {
        end,
        target_released,
        status,
        settlement,
        stderr,
        stderr_capped,
    }
}

#[cfg(test)]
mod tests;
