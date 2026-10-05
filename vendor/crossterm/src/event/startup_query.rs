//! Astra's local startup-query extension. See ASTRA-PATCH.md.
use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{filter::Filter, lock_internal_event_reader, InternalEvent, KeyboardEnhancementFlags};

/// Raw responses to a bounded, one-shot terminal startup query.
#[derive(Debug, Default)]
pub struct StartupAttributes {
    /// OSC 10 response including its framing.
    pub foreground: Option<String>,
    /// OSC 11 response including its framing.
    pub background: Option<String>,
    /// DA1 parameters, including the terminal class as the first parameter.
    pub device_attributes: Option<Vec<u16>>,
    /// Present only if a keyboard-enhancement reply arrived before DA1, per
    /// <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>.
    /// `None` means unsupported (or a reply lost to the same bounded
    /// deadline as the other fields here) -- never re-queried on its own,
    /// for the same reason a missing DA1 is not retried: a second `CSI ?u`
    /// would be a competing query outside this one bounded round trip.
    pub keyboard_enhancement_flags: Option<KeyboardEnhancementFlags>,
}

static DEVICE_ATTRIBUTES: OnceLock<Vec<u16>> = OnceLock::new();

/// The first DA1 response seen by the shared reader, including a late reply.
/// This accessor never reads terminal input or initiates a query.
pub fn cached_primary_device_attributes() -> Option<&'static [u16]> {
    DEVICE_ATTRIBUTES.get().map(Vec::as_slice)
}

pub(super) fn record_device_attributes(params: &[u16]) {
    let _ = DEVICE_ATTRIBUTES.set(params.to_vec());
}

struct StartupFilter;
impl Filter for StartupFilter {
    fn eval(&self, event: &InternalEvent) -> bool {
        matches!(
            event,
            InternalEvent::OscResponse(_)
                | InternalEvent::PrimaryDeviceAttributes(_)
                | InternalEvent::KeyboardEnhancementFlags(_)
        )
    }
}

/// Query colors (optionally), keyboard-enhancement support, and DA1 through
/// the existing input reader.
///
/// Call in raw mode, before creating an EventStream. Unrelated events remain
/// queued in their original order. Late replies remain internal events.
/// The caller owns terminal modes and decides how to handle missing responses.
///
/// `CSI ?u` is written before `CSI c` and shares DA1 as its completion
/// sentinel, exactly like the recommended detection method in the Kitty
/// keyboard-protocol spec: a supporting terminal answers `?u` at or before
/// its DA1 reply, so once DA1 arrives, a still-missing flags reply means the
/// terminal does not support the protocol. This keeps every startup query in
/// the same single bounded round trip instead of a second `CSI ?u`/`CSI c`
/// pair racing this one.
pub fn query_startup_attributes(colors: bool, timeout: Duration) -> io::Result<StartupAttributes> {
    let mut reader = lock_internal_event_reader();
    reader.set_startup_query(true);
    let result = (|| {
        let mut stdout = io::stdout().lock();
        if colors {
            stdout.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\")?;
        }
        stdout.write_all(b"\x1b[?u\x1b[c")?;
        stdout.flush()?;
        drop(stdout);

        let deadline = Instant::now() + timeout;
        let mut result = StartupAttributes::default();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if !reader.poll(Some(remaining), &StartupFilter)? {
                break;
            }
            match reader.read(&StartupFilter)? {
                InternalEvent::OscResponse(response) if colors => {
                    if response.starts_with("\x1b]10;") {
                        result.foreground = Some(response);
                    } else if response.starts_with("\x1b]11;") {
                        result.background = Some(response);
                    }
                }
                InternalEvent::PrimaryDeviceAttributes(params) => {
                    result.device_attributes = Some(params);
                }
                InternalEvent::KeyboardEnhancementFlags(flags) => {
                    result.keyboard_enhancement_flags = Some(flags);
                }
                _ => {}
            }
            if result.device_attributes.is_some()
                && (!colors || (result.foreground.is_some() && result.background.is_some()))
            {
                break;
            }
        }
        Ok(result)
    })();
    // Reset even when writing or reading failed. Normal-session Esc must keep
    // the upstream immediate-delivery semantics.
    reader.set_startup_query(false);
    result
}

struct ResizeFilter;
impl Filter for ResizeFilter {
    fn eval(&self, event: &InternalEvent) -> bool {
        matches!(event, InternalEvent::Event(super::Event::Resize(_, _)))
    }
}

/// Cursor reply and the terminal dimensions for which it was requested.
#[derive(Debug)]
pub struct CursorPositionReport {
    /// Dimensions sampled after draining old resize notifications.
    pub size: (u16, u16),
    /// None when the terminal did not answer within the query deadline.
    pub position: Option<(u16, u16)>,
    /// Another resize arrived while this reply was in flight. Its event is
    /// left queued; the caller may track movement but must defer painting.
    pub interrupted: bool,
}

/// Query the cursor through the shared reader with a bounded deadline.
///
/// The caller must pause/drop its EventStream first. Unrelated input remains
/// queued in FIFO order; stale CPR replies are discarded before issuing DSR.
/// This does not change terminal modes or create another terminal reader.
pub fn query_cursor_position(timeout: Duration) -> io::Result<CursorPositionReport> {
    use super::filter::CursorPositionFilter;

    let mut reader = lock_internal_event_reader();
    reader.set_startup_query(true);
    let result = (|| {
        // Older SIGWINCH notifications do not invalidate the query we are
        // about to issue. Keep keyboard/paste input in the shared FIFO.
        while reader.poll(Some(Duration::ZERO), &ResizeFilter)? {
            reader.read(&ResizeFilter)?;
        }
        while reader.poll(Some(Duration::ZERO), &CursorPositionFilter)? {
            reader.read(&CursorPositionFilter)?;
            reader.cursor_query_pending = false;
        }
        let size = crate::terminal::size()?;
        // CPR has no request identifier. A reply still in flight after a
        // deadline cannot be attributed to a new query. Keep the one existing
        // request outstanding until its reply has been consumed above.
        if reader.cursor_query_pending {
            return Ok(CursorPositionReport {
                size,
                position: None,
                interrupted: reader.poll(Some(Duration::ZERO), &ResizeFilter)?,
            });
        }
        reader.cursor_query_pending = true;
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\x1b[6n")?;
        stdout.flush()?;
        drop(stdout);

        let deadline = Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if reader.poll(Some(remaining), &CursorPositionFilter)? {
                if let InternalEvent::CursorPosition(x, y) = reader.read(&CursorPositionFilter)? {
                    reader.cursor_query_pending = false;
                    return Ok(CursorPositionReport {
                        size,
                        position: Some((x, y)),
                        interrupted: reader.poll(Some(Duration::ZERO), &ResizeFilter)?,
                    });
                }
            }
        }
        Ok(CursorPositionReport {
            size,
            position: None,
            interrupted: reader.poll(Some(Duration::ZERO), &ResizeFilter)?,
        })
    })();
    reader.set_startup_query(false);
    result
}
