//! Bounded byte-stream and newline-delimited I/O on one invocation owner.
//! Admission, JSON interpretation, native sessions and requests belong to the
//! selected CLI/User Runner adapter, not this physical process boundary.

use super::{
    BashInvocationOwner, InvocationSupervisor, IsolatedScopeAbortGuard, OUTPUT_DRAIN_TIMEOUT,
    READ_CHUNK_SIZE, ScopeSettlement, apply_process_scope, reap_child_after_owner_loss,
    settle_isolated_owner_after_exit, terminate_isolated_child,
};
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Explicit resource budget for one physical process, not a logical session.
#[derive(Debug, Clone, Copy)]
pub struct ProcessIoLimits {
    /// Payload bytes per stdin/stdout frame, excluding the LF delimiter.
    pub max_frame_bytes: usize,
    /// Capacity of each input/output queue. Slow consumers apply backpressure.
    pub max_queued_frames: usize,
    /// Retained stderr prefix; excess diagnostics are drained, not accumulated.
    pub max_stderr_bytes: usize,
    /// Optional physical invocation cutoff. The ownership handshake is bounded independently.
    pub timeout: Option<Duration>,
}

impl ProcessIoLimits {
    fn deadline(self) -> io::Result<Option<Instant>> {
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
            || self.timeout.is_some_and(|timeout| timeout.is_zero())
            || buffered.is_none_or(|bytes| bytes > 64 * 1024 * 1024)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid framed process budget",
            ));
        }
        self.timeout
            .map(|timeout| {
                Instant::now().checked_add(timeout).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "process deadline overflow")
                })
            })
            .transpose()
    }
}

/// Cloneable input endpoint. A successful send means queued, not written or
/// accepted by the native protocol. Native request/response IDs provide that
/// acknowledgement. No caller-owned payload is copied until a slot is reserved.
#[derive(Debug, Clone)]
pub struct ProcessInput {
    sender: mpsc::Sender<InputWrite>,
    cancel: CancellationToken,
    max_frame_bytes: usize,
    framed: bool,
}

#[derive(Debug)]
pub enum ProcessInputError {
    NotSent(io::Error),
    Unconfirmed(io::Error),
}

impl std::fmt::Display for ProcessInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(error) => write!(f, "input was not sent: {error}"),
            Self::Unconfirmed(error) => write!(f, "input delivery is unconfirmed: {error}"),
        }
    }
}

impl std::error::Error for ProcessInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::NotSent(error) | Self::Unconfirmed(error) => error,
        })
    }
}

#[derive(Debug)]
struct InputWrite {
    bytes: Option<Vec<u8>>,
    written: Option<oneshot::Sender<io::Result<()>>>,
}

impl ProcessInput {
    pub fn is_open(&self) -> bool {
        !self.sender.is_closed() && !self.cancel.is_cancelled()
    }

    pub async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
        if !self.framed
            || frame.is_empty()
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
                permit.send(InputWrite { bytes: Some(frame.to_vec()), written: None });
                Ok(())
            }
        }
    }

    /// Acknowledges bytes written to the pipe, not processing by the child.
    /// Cancellation or a lost acknowledgement never authorizes replay.
    pub async fn write_bytes(
        &self,
        bytes: &[u8],
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), ProcessInputError> {
        if self.framed || bytes.len() > self.max_frame_bytes {
            return Err(ProcessInputError::NotSent(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid process input",
            )));
        }
        self.write(Some(bytes), deadline, cancel).await
    }

    pub async fn close(
        &self,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), ProcessInputError> {
        self.write(None, deadline, cancel).await
    }

    async fn write(
        &self,
        bytes: Option<&[u8]>,
        deadline: Instant,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), ProcessInputError> {
        let permit = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(ProcessInputError::NotSent(io::Error::new(io::ErrorKind::Interrupted, "process cancelled"))),
            _ = async { match cancel { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => return Err(ProcessInputError::NotSent(io::Error::new(io::ErrorKind::Interrupted, "input cancelled"))),
            _ = tokio::time::sleep_until(deadline) => return Err(ProcessInputError::NotSent(io::Error::new(io::ErrorKind::TimedOut, "input deadline elapsed before submission"))),
            permit = self.sender.reserve() => permit.map_err(|_| ProcessInputError::NotSent(io::Error::new(io::ErrorKind::BrokenPipe, "process input closed")))?,
        };
        let (written, acknowledgement) = oneshot::channel();
        permit.send(InputWrite {
            bytes: bytes.map(<[u8]>::to_vec),
            written: Some(written),
        });
        tokio::select! {
            biased;
            result = acknowledgement => result
                .map_err(|_| ProcessInputError::Unconfirmed(io::Error::other("write acknowledgement was lost")))?
                .map_err(ProcessInputError::Unconfirmed),
            _ = async { match cancel { Some(cancel) => cancel.cancelled().await, None => std::future::pending().await } } => Err(ProcessInputError::Unconfirmed(io::Error::new(io::ErrorKind::Interrupted, "input acknowledgement wait cancelled"))),
            _ = tokio::time::sleep_until(deadline) => Err(ProcessInputError::Unconfirmed(io::Error::new(io::ErrorKind::TimedOut, "input acknowledgement deadline elapsed"))),
        }
    }
}

#[derive(Debug)]
pub enum ProcessOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

#[derive(Clone, Copy)]
enum IoMode {
    Framed,
    Bytes { stdin: bool },
}

/// Physical transport stop cause; none of these implies a native turn outcome.
#[derive(Debug)]
pub enum ProcessEnd {
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
pub struct ProcessOutcome {
    pub end: ProcessEnd,
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
pub struct OwnedProcess {
    input: Option<ProcessInput>,
    output: mpsc::Receiver<ProcessOutput>,
    completion: Option<JoinHandle<ProcessOutcome>>,
    cancel: CancellationToken,
}

impl OwnedProcess {
    /// Input is available for framed processes and shells launched with stdin.
    pub fn input(&self) -> Option<ProcessInput> {
        self.input.clone()
    }

    /// Complete LF-delimited stdout payloads only. EOF or a closed queue is not
    /// a native success: inspect the final outcome and the adapter's protocol.
    pub async fn recv_frame(&mut self) -> Option<Vec<u8>> {
        while let Some(output) = self.output.recv().await {
            if let ProcessOutput::Stdout(bytes) = output {
                return Some(bytes);
            }
        }
        None
    }

    pub async fn recv_output(&mut self) -> Option<ProcessOutput> {
        self.output.recv().await
    }

    /// Cancels this invocation, never the caller's parent cancellation token.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Close this handle's input and drain/discard remaining frames while
    /// waiting. Retained input clones must also be dropped for stdin EOF.
    /// Consumers that need all evidence must receive it before calling wait.
    pub async fn wait(mut self) -> Result<ProcessOutcome, tokio::task::JoinError> {
        self.input.take();
        while self.output.recv().await.is_some() {}
        self.completion.take().expect("process completion").await
    }

    pub async fn cancel_and_wait(self) -> Result<ProcessOutcome, tokio::task::JoinError> {
        self.cancel();
        self.wait().await
    }
}

impl Drop for OwnedProcess {
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
        command: Command,
        limits: ProcessIoLimits,
        cancel: CancellationToken,
    ) -> io::Result<OwnedProcess> {
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
        self.spawn_io(command, limits, cancel, IoMode::Framed, ())
    }

    /// Raw shell I/O shares the exact same deadline, cancellation and cleanup
    /// driver as structured collaborators. Weak owners never mint settlement.
    pub fn spawn_streamed<C: Send + 'static>(
        self,
        command: Command,
        limits: ProcessIoLimits,
        cancel: CancellationToken,
        stdin: bool,
        custody: C,
    ) -> io::Result<OwnedProcess> {
        self.spawn_io(command, limits, cancel, IoMode::Bytes { stdin }, custody)
    }

    fn spawn_io<C: Send + 'static>(
        self,
        mut command: Command,
        limits: ProcessIoLimits,
        cancel: CancellationToken,
        mode: IoMode,
        custody: C,
    ) -> io::Result<OwnedProcess> {
        let deadline = limits.deadline()?;
        tokio::runtime::Handle::try_current()
            .map_err(|error| io::Error::other(format!("framed process requires Tokio: {error}")))?;
        if cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "process cancelled before spawn",
            ));
        }
        command
            .stdin(if matches!(mode, IoMode::Bytes { stdin: false }) {
                Stdio::null()
            } else {
                Stdio::piped()
            })
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
        let input = ProcessInput {
            sender: input_tx,
            cancel: cancel.clone(),
            max_frame_bytes: limits.max_frame_bytes,
            framed: matches!(mode, IoMode::Framed),
        };
        let completion = tokio::spawn(drive(
            child,
            guard,
            input_rx,
            output_tx,
            limits,
            deadline,
            cancel.clone(),
            mode,
            custody,
        ));
        Ok(OwnedProcess {
            input: (!matches!(mode, IoMode::Bytes { stdin: false })).then_some(input),
            output: output_rx,
            completion: Some(completion),
            cancel,
        })
    }
}

async fn read_frames(
    mut reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<ProcessOutput>,
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
                sender
                    .send(ProcessOutput::Stdout(std::mem::take(&mut frame)))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "stdout consumer closed")
                    })?;
            }
        }
    }
}

async fn write_frames(
    mut writer: impl AsyncWrite + Unpin,
    mut receiver: mpsc::Receiver<InputWrite>,
    framed: bool,
) -> io::Result<()> {
    while let Some(request) = receiver.recv().await {
        // A cancelled caller has not authorized a queued write that has not
        // started yet. An in-flight write can still be partial/unknown.
        if request
            .written
            .as_ref()
            .is_some_and(oneshot::Sender::is_closed)
        {
            continue;
        }
        let close = request.bytes.is_none();
        let result = async {
            if let Some(bytes) = request.bytes {
                writer.write_all(&bytes).await?;
                if framed {
                    writer.write_all(b"\n").await?;
                }
                writer.flush().await
            } else {
                writer.shutdown().await
            }
        }
        .await;
        let failed = result
            .as_ref()
            .err()
            .map(|error| io::Error::new(error.kind(), error.to_string()));
        if let Some(written) = request.written {
            let _ = written.send(result);
        }
        if let Some(error) = failed {
            return Err(error);
        }
        if close {
            return Ok(());
        }
    }
    writer.shutdown().await
}

async fn read_bytes(
    mut reader: impl AsyncRead + Unpin,
    sender: mpsc::Sender<ProcessOutput>,
    stderr: bool,
    limit: usize,
) -> io::Result<()> {
    let mut buffer = vec![0; READ_CHUNK_SIZE.min(limit)];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let bytes = buffer[..count].to_vec();
        let output = if stderr {
            ProcessOutput::Stderr(bytes)
        } else {
            ProcessOutput::Stdout(bytes)
        };
        sender.send(output).await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "process output consumer closed")
        })?;
    }
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

// Keep the private invocation's ownership transfer explicit; a parameter-only
// wrapper would not add another lifecycle or useful abstraction.
#[allow(clippy::too_many_arguments)]
async fn drive<C: Send + 'static>(
    mut child: tokio::process::Child,
    guard: IsolatedScopeAbortGuard,
    input: mpsc::Receiver<InputWrite>,
    output: mpsc::Sender<ProcessOutput>,
    limits: ProcessIoLimits,
    deadline: Option<Instant>,
    cancel: CancellationToken,
    mode: IoMode,
    custody: C,
) -> ProcessOutcome {
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
            } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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
                    .started_before(
                        pid,
                        deadline
                            .unwrap_or_else(|| Instant::now() + super::SUPERVISOR_HANDSHAKE_TIMEOUT)
                            .into_std(),
                        &check_start,
                    )
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
        .and_then(super::InvocationSupervisor::target_released)
        .or_else(|| (!supervised && startup_error.is_none()).then_some(true));
    let end = if let Some(error) = startup_error {
        match error.kind() {
            io::ErrorKind::Interrupted => ProcessEnd::Cancelled,
            io::ErrorKind::TimedOut => ProcessEnd::TimedOut,
            _ => ProcessEnd::OwnershipFailed(error),
        }
    } else {
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take();
        let stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr_output = output.clone();
        let stdout = async {
            match mode {
                IoMode::Framed => read_frames(stdout, output, limits.max_frame_bytes).await,
                IoMode::Bytes { .. } => {
                    read_bytes(stdout, output, false, limits.max_frame_bytes).await
                }
            }
        };
        let stdin = async {
            match stdin {
                Some(stdin) => write_frames(stdin, input, matches!(mode, IoMode::Framed)).await,
                None => Ok(()),
            }
        };
        let stderr_reader = async {
            match mode {
                IoMode::Framed => {
                    drain_stderr(
                        stderr_pipe,
                        &mut stderr,
                        &mut stderr_capped,
                        limits.max_stderr_bytes,
                    )
                    .await
                }
                IoMode::Bytes { .. } => {
                    read_bytes(stderr_pipe, stderr_output, true, limits.max_frame_bytes).await
                }
            }
        };
        tokio::pin!(stdout, stdin, stderr_reader);
        let (mut stdout_done, mut stdin_done, mut stderr_done) = (false, false, false);
        let mut drain_deadline = None;
        loop {
            if status.is_some() && stdout_done && stderr_done {
                break ProcessEnd::Exited;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break ProcessEnd::Cancelled,
                _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => break ProcessEnd::TimedOut,
                _ = async { if let Some(deadline) = drain_deadline { tokio::time::sleep_until(deadline).await } }, if drain_deadline.is_some() => {
                    break ProcessEnd::OutputDrainTimedOut;
                }
                result = &mut stdout, if !stdout_done => {
                    match result { Ok(()) => stdout_done = true, Err(error) => break ProcessEnd::OutputFailed(error) }
                }
                result = &mut stdin, if !stdin_done => {
                    match result {
                        Ok(()) => stdin_done = true,
                        // A shell may close stdin while continuing useful
                        // work. The write caller receives its I/O error; that
                        // rejected input is not authority to kill the task.
                        Err(_) if matches!(mode, IoMode::Bytes { .. }) => stdin_done = true,
                        Err(error) => break ProcessEnd::InputFailed(error),
                    }
                }
                result = &mut stderr_reader, if !stderr_done => {
                    match result { Ok(()) => stderr_done = true, Err(error) => break ProcessEnd::StderrFailed(error) }
                }
                result = child.wait(), if status.is_none() => {
                    match result {
                        Ok(exit) => status = Some(exit),
                        Err(error) => break ProcessEnd::WaitFailed(error),
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
            reap_child_after_owner_loss(&mut child, supervised).await;
        }
        status = child.try_wait().ok().flatten();
        guard.disarm();
    }
    // Workspace leases and pinned resources outlive cancellation, pipe drain
    // and descendant cleanup even when the presentation-side handle is lost.
    drop(custody);
    ProcessOutcome {
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
