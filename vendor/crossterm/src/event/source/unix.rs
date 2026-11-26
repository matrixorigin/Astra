use super::InputBatch;
use crate::terminal::sys::file_descriptor::FileDesc;
use std::io;

mod parser;
#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;

/// Check readiness before every read, including drain reads after an incomplete
/// escape sequence. stdin is normally blocking; a second unconditional read
/// would bypass the parser deadline. The event reader is the sole input owner.
fn read_ready(
    fd: &crate::terminal::sys::file_descriptor::FileDesc<'_>,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    let mut fds = [filedescriptor::pollfd {
        fd: fd.raw_fd(),
        events: filedescriptor::POLLIN,
        revents: 0,
    }];
    match filedescriptor::poll(&mut fds, Some(std::time::Duration::ZERO)) {
        Ok(_) if fds[0].revents == 0 => Err(std::io::ErrorKind::WouldBlock.into()),
        Ok(_) => fd.read(buffer),
        Err(filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error)) => Err(error),
        Err(error) => Err(std::io::Error::new(std::io::ErrorKind::Other, error)),
    }
}

/// Read from the current terminal batch, leaving newer bytes for the next
/// timestamp. Only metadata is retained: the existing parser owns the data.
fn read_input(
    fd: &FileDesc<'_>,
    buffer: &mut [u8],
    batch: &mut InputBatch,
    parser: &mut parser::Parser,
) -> io::Result<()> {
    let limit = batch
        .0
        .map_or(buffer.len(), |(_, count)| count.min(buffer.len()));
    match read_ready(fd, &mut buffer[..limit]) {
        Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
        Ok(count) => {
            let received_at = batch.consume(count, || fd.queued_input());
            parser.advance_at(
                &buffer[..count],
                count == buffer.len() || batch.0.is_some(),
                received_at,
            );
            Ok(())
        }
        Err(error) => {
            if error.kind() != io::ErrorKind::Interrupted {
                *batch = InputBatch::default();
            }
            Err(error)
        }
    }
}
