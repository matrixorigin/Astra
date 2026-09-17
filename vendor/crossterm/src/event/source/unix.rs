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
