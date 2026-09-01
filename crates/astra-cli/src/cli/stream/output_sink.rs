//! Process-output boundary for non-interactive CLI protocols.
//!
//! Rust starts with `SIGPIPE` ignored so writes to a closed pipe return
//! `BrokenPipe`. Keep that recoverable contract process-wide: Astra also uses
//! pipes internally for child-process control and input, where signal death
//! would bypass typed cleanup and remote-run cancellation.

use std::fmt;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicU8, Ordering};

const STATE_OPEN: u8 = 0;
const STATE_CLOSED: u8 = 1;
const STATE_FAILED: u8 = 2;

static STDOUT_STATE: AtomicU8 = AtomicU8::new(STATE_OPEN);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutState {
    Open,
    Closed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputWriteStatus {
    Written,
    Closed,
}

/// Establish the one process-wide Unix pipe policy before application threads
/// start. Reasserting `SIG_IGN` makes the invariant explicit for embedders and
/// tests without creating a command- or thread-local signal mode.
#[cfg(unix)]
pub fn configure_process_output_signals() -> Result<(), nix::Error> {
    use nix::sys::signal::{SigHandler, Signal, signal};

    // SAFETY: main calls this before logging, the Tokio runtime, or any other
    // application thread. `SIG_IGN` is the Rust runtime's normal SIGPIPE
    // contract and turns closed pipes into ordinary I/O errors.
    unsafe { signal(Signal::SIGPIPE, SigHandler::SigIgn) }?;
    Ok(())
}

#[cfg(not(unix))]
pub fn configure_process_output_signals() -> Result<(), std::convert::Infallible> {
    Ok(())
}

pub fn stdout_was_closed() -> bool {
    stdout_state() == StdoutState::Closed
}

pub fn stdout_state() -> StdoutState {
    match STDOUT_STATE.load(Ordering::Acquire) {
        STATE_CLOSED => StdoutState::Closed,
        STATE_FAILED => StdoutState::Failed,
        _ => StdoutState::Open,
    }
}

pub fn resolved_exit_code(exit_code: i32) -> i32 {
    if exit_code != 0 {
        return exit_code;
    }
    match stdout_state() {
        StdoutState::Open => 0,
        StdoutState::Closed => 128 + 13,
        StdoutState::Failed => 3,
    }
}

pub fn write_stdout(bytes: &[u8]) -> io::Result<OutputWriteStatus> {
    if let Some(status) = prior_write_status()? {
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match lock.write_all(bytes).and_then(|()| lock.flush()) {
        Ok(()) => Ok(OutputWriteStatus::Written),
        Err(error) => classify_stdout_error(error),
    }
}

pub fn write_stdout_line(text: &str) -> io::Result<OutputWriteStatus> {
    write_stdout_fragments(&[text.as_bytes(), b"\n"])
}

/// Preserve pre-rendered bytes while exposing logical line boundaries to a
/// downstream consumer. Clap help/version output uses this so a pager or
/// `head` closure is observable without timing tricks or pipeline inspection.
pub fn write_stdout_records(text: &str) -> io::Result<OutputWriteStatus> {
    for record in text.split_inclusive('\n') {
        let status = write_stdout(record.as_bytes())?;
        if status == OutputWriteStatus::Closed {
            return Ok(status);
        }
    }
    Ok(OutputWriteStatus::Written)
}

pub fn write_stdout_fragments(fragments: &[&[u8]]) -> io::Result<OutputWriteStatus> {
    if let Some(status) = prior_write_status()? {
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    for fragment in fragments {
        if let Err(error) = lock.write_all(fragment) {
            return classify_stdout_error(error);
        }
    }
    match lock.flush() {
        Ok(()) => Ok(OutputWriteStatus::Written),
        Err(error) => classify_stdout_error(error),
    }
}

pub fn write_stdout_operation(
    operation: impl FnOnce(&mut io::StdoutLock<'_>) -> io::Result<()>,
) -> io::Result<OutputWriteStatus> {
    if let Some(status) = prior_write_status()? {
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match operation(&mut lock) {
        Ok(()) => Ok(OutputWriteStatus::Written),
        Err(error) => classify_stdout_error(error),
    }
}

/// Allocation-free formatted output for human-readable commands. This keeps
/// `stdout_print!`'s flush cadence; immediate machine protocols use the byte/line
/// helpers above, which flush once per logical record.
pub fn write_stdout_fmt(args: fmt::Arguments<'_>) -> io::Result<OutputWriteStatus> {
    write_stdout_fmt_inner(args, false)
}

pub fn write_stdout_fmt_line(args: fmt::Arguments<'_>) -> io::Result<OutputWriteStatus> {
    write_stdout_fmt_inner(args, true)
}

pub fn flush_stdout() -> io::Result<OutputWriteStatus> {
    if let Some(status) = prior_write_status()? {
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match lock.flush() {
        Ok(()) => Ok(OutputWriteStatus::Written),
        Err(error) => classify_stdout_error(error),
    }
}

fn write_stdout_fmt_inner(
    args: fmt::Arguments<'_>,
    newline: bool,
) -> io::Result<OutputWriteStatus> {
    if let Some(status) = prior_write_status()? {
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    if let Err(error) = lock.write_fmt(args) {
        return classify_stdout_error(error);
    }
    if newline && let Err(error) = lock.write_all(b"\n") {
        return classify_stdout_error(error);
    }
    Ok(OutputWriteStatus::Written)
}

fn prior_write_status() -> io::Result<Option<OutputWriteStatus>> {
    match stdout_state() {
        StdoutState::Open => Ok(None),
        StdoutState::Closed => Ok(Some(OutputWriteStatus::Closed)),
        StdoutState::Failed => Err(io::Error::other("stdout previously failed")),
    }
}

fn classify_stdout_error(error: io::Error) -> io::Result<OutputWriteStatus> {
    if error.kind() == io::ErrorKind::BrokenPipe {
        let _ = STDOUT_STATE.compare_exchange(
            STATE_OPEN,
            STATE_CLOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(OutputWriteStatus::Closed)
    } else {
        STDOUT_STATE.store(STATE_FAILED, Ordering::Release);
        Err(error)
    }
}

pub fn write_stderr_line(text: &str) -> io::Result<OutputWriteStatus> {
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    match lock
        .write_all(text.as_bytes())
        .and_then(|()| lock.write_all(b"\n"))
        .and_then(|()| lock.flush())
    {
        Ok(()) => Ok(OutputWriteStatus::Written),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(OutputWriteStatus::Closed),
        Err(error) => Err(error),
    }
}
