//! display_sixel — render an image file to the terminal via sixel.
//!
//! This tool converts a PNG/JPEG/GIF/etc. image to sixel escape sequences
//! using `img2sixel` (part of libsixel) and shows it inline in a sixel-capable
//! terminal (e.g. mlterm, xterm -ti vt340, foot, WezTerm, iTerm2, contour,
//! Windows Terminal ≥1.22).
//!
//! # Two output paths
//!
//! The naive approach — write the sixel bytes straight to `/dev/tty` — does not
//! work under the interactive `astra` TUI. That TUI is a Codex-style *inline
//! viewport* (raw mode, no alternate screen): it owns the bottom rows of the
//! screen and repaints them on its own schedule. Anything written to the tty
//! from inside a tool lands in/around that viewport and is immediately painted
//! over on the next frame — the user sees a blank/white box, not the image.
//!
//! So we split by context, distinguished by [`set_tui_active`]:
//!
//! * **TUI active** — the tool renders the image with `img2sixel` and *queues*
//!   the raw bytes ([`take_pending_sixel`]). The TUI event loop drains the queue
//!   and blits each image while the render loop is paused via
//!   `TerminalGuard::with_restored`, which disables raw mode, hands the screen to
//!   the image, waits for the user, then forces a clean full repaint. This
//!   reuses the same battle-tested pause/restore path slash commands use.
//! * **TUI inactive** (headless / scripting) — stdout *is* the real terminal, so
//!   the tool writes the sixel bytes straight to the controlling terminal.
//!
//! # Pre-conditions
//! 1. `img2sixel` must be on PATH.
//! 2. The terminal must understand sixel graphics. We do not probe for this —
//!    reliable detection requires a DA1 round-trip that is impractical here and
//!    the previous heuristics produced false positives. The tool is opt-in (an
//!    agent calls it after producing an image), so we simply attempt to render.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::ToolResult;

/// True while the interactive `astra` TUI owns the terminal. The TUI flips this
/// on at startup and off at teardown (see `TerminalGuard`). When set,
/// `display_sixel` queues image bytes for the TUI instead of writing to the tty.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Cached sixel-capability of the controlling terminal, as a tri-state:
/// `0` unknown, `1` supported, `2` unsupported. The TUI probes once at startup
/// (see [`set_sixel_supported`] / [`probe_sixel_support`]) and stores the result
/// here so `display_sixel` never enters a modal on a terminal that can't render
/// the image.
static SIXEL_CAP: AtomicU8 = AtomicU8::new(0);

/// Record the probed sixel capability of the terminal. Called by the CLI's
/// `TerminalGuard` at startup.
pub fn set_sixel_supported(supported: bool) {
    SIXEL_CAP.store(if supported { 1 } else { 2 }, Ordering::SeqCst);
}

/// Read the cached sixel capability, or `None` if it has not been probed yet.
fn cached_sixel_support() -> Option<bool> {
    match SIXEL_CAP.load(Ordering::SeqCst) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Raw sixel byte buffers produced while the TUI is active, awaiting the TUI
/// event loop to blit them onto a paused screen.
static PENDING_SIXEL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Record whether the interactive TUI owns the terminal. Called by the CLI's
/// `TerminalGuard` on init (`true`) and drop (`false`).
pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::SeqCst);
}

/// Drain the queued sixel buffers. The TUI calls this and renders each buffer
/// while the terminal is paused. Returns the buffers in submission order.
pub fn take_pending_sixel() -> Vec<Vec<u8>> {
    let mut pending = PENDING_SIXEL.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *pending)
}

fn queue_pending_sixel(bytes: Vec<u8>) {
    let mut pending = PENDING_SIXEL.lock().unwrap_or_else(|e| e.into_inner());
    pending.push(bytes);
}

/// Check whether `img2sixel` is on PATH.
fn img2sixel_available() -> bool {
    which::which("img2sixel").is_ok()
}

/// Ask the controlling terminal whether it supports sixel graphics.
///
/// Sends a DA1 (Primary Device Attributes) request `ESC [ c`; a sixel-capable
/// terminal replies `ESC [ ? … ; 4 ; … c` where attribute `4` is Sixel Graphics.
/// Returns `false` on no `/dev/tty`, no reply within the timeout, or a reply that
/// omits attribute `4`.
///
/// MUST be called while nothing else is reading the terminal (e.g. before the
/// TUI's input reader starts) — it reads the reply bytes directly.
#[cfg(unix)]
pub fn probe_sixel_support() -> bool {
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return false;
    };
    let fd = tty.as_raw_fd();

    // Save termios and switch to raw so the reply (which has no newline) is
    // readable byte-by-byte instead of waiting for line discipline.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return false;
    }
    let mut raw = saved;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return false;
    }

    let result = (|| {
        tty.write_all(b"\x1b[c").ok()?;
        tty.flush().ok()?;

        // Poll for the reply, up to ~300ms total, stopping at the `c` terminator.
        let mut buf: Vec<u8> = Vec::with_capacity(64);
        let mut chunk = [0u8; 32];
        for _ in 0..20 {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let r = unsafe { libc::poll(&mut pfd, 1, 15) };
            if r < 0 {
                return None;
            }
            if r == 0 {
                continue; // nothing yet — keep waiting within the budget
            }
            match tty.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.contains(&b'c') {
                        return Some(parse_da1_has_sixel(&buf));
                    }
                }
                Err(_) => return None,
            }
        }
        None
    })();

    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    result.unwrap_or(false)
}

#[cfg(not(unix))]
pub fn probe_sixel_support() -> bool {
    false
}

/// Parse a DA1 reply `ESC [ ? Ps ; Ps ; … c` and report whether attribute `4`
/// (Sixel Graphics) is present.
fn parse_da1_has_sixel(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    let Some(start) = s.find("\x1b[?") else {
        return false;
    };
    let rest = &s[start + 3..];
    let Some(end) = rest.find('c') else {
        return false;
    };
    rest[..end].split(';').any(|p| p.trim() == "4")
}

/// Write raw sixel bytes to the controlling terminal.
///
/// Uses `/dev/tty` rather than stdout so a redirected stdout still renders the
/// image. Only used when the TUI is *not* active.
fn write_sixel_to_terminal(bytes: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    let mut tty = OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(bytes)?;
    tty.flush()
}

/// Render an image file to the terminal via sixel.
///
/// Returns a `ToolResult`: a short status line on success, or a descriptive
/// error (missing file, `img2sixel` not installed, conversion failure, or a
/// tty write failure).
pub fn display_sixel(path: &str) -> ToolResult {
    // 1. Validate the file exists and is a regular file.
    let file_path = Path::new(path);
    if !file_path.exists() {
        return ToolResult::error(format!("display_sixel: file not found: {path}"));
    }
    if !file_path.is_file() {
        return ToolResult::error(format!("display_sixel: not a regular file: {path}"));
    }

    // 2. Bail early if the terminal can't render sixel — better a clear message
    //    than a blank modal. Use the value the TUI probed at startup; in headless
    //    mode there's no cached value, so probe now (safe: no concurrent reader).
    let supported = cached_sixel_support().unwrap_or_else(probe_sixel_support);
    if !supported {
        return ToolResult::text(format!(
            "display_sixel: this terminal does not support sixel graphics, \
             so the image was not displayed ({path})."
        ));
    }

    // 3. Check img2sixel is installed.
    if !img2sixel_available() {
        return ToolResult::error(
            "display_sixel: img2sixel is not installed.\n\
             Install it with your package manager:\n\
             • Debian/Ubuntu: sudo apt install libsixel-bin\n\
             • Fedora:        sudo dnf install libsixel\n\
             • Arch:          sudo pacman -S libsixel\n\
             • macOS:         brew install libsixel\n\
             Or build from source: https://github.com/saitoha/libsixel"
                .to_string(),
        );
    }

    // 4. Convert the image to sixel.
    let output = match Command::new("img2sixel").arg(file_path).output() {
        Ok(o) => o,
        Err(e) => {
            return ToolResult::error(format!("display_sixel: failed to run img2sixel: {e}"));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ToolResult::error(format!(
            "display_sixel: img2sixel failed with status {}:\n{stderr}",
            output.status
        ));
    }
    if output.stdout.is_empty() {
        return ToolResult::error(
            "display_sixel: img2sixel produced no output (unsupported or corrupt image?)"
                .to_string(),
        );
    }

    // 5. Route the bytes based on who owns the terminal.
    if TUI_ACTIVE.load(Ordering::SeqCst) {
        // Hand off to the TUI event loop, which pauses the render loop and
        // blits the image on a clean screen. Writing to the tty ourselves here
        // would just be painted over by the next frame.
        queue_pending_sixel(output.stdout);
        return ToolResult::text(format!(
            "Rendering {path} in the terminal — press Enter after viewing to continue."
        ));
    }

    // Non-TUI (headless / scripting): stdout is the real terminal.
    match write_sixel_to_terminal(&output.stdout) {
        Ok(()) => ToolResult::text(format!("Displayed image: {path}")),
        Err(e) => ToolResult::error(format!(
            "display_sixel: failed to write sixel data to the terminal: {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_img2sixel_available_does_not_panic() {
        let _ = img2sixel_available();
    }

    #[test]
    fn test_missing_file_returns_error() {
        let result = display_sixel("/tmp/definitely_nonexistent_file_42.png");
        assert!(result.is_error);
        assert!(result.output.contains("file not found"));
    }

    #[test]
    fn test_directory_returns_error() {
        let result = display_sixel("/tmp");
        assert!(result.is_error);
        assert!(result.output.contains("not a regular file"));
    }

    #[test]
    fn test_pending_sixel_queue_roundtrip() {
        // Draining an empty queue yields nothing.
        let _ = take_pending_sixel();
        queue_pending_sixel(b"\x1bPq#0;2;100;0;0#0!10~-\x1b\\".to_vec());
        queue_pending_sixel(b"second".to_vec());
        let drained = take_pending_sixel();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[1], b"second");
        // Second drain is empty — take() cleared it.
        assert!(take_pending_sixel().is_empty());
    }

    #[test]
    fn test_parse_da1_has_sixel() {
        // xterm with sixel: attribute 4 present.
        assert!(parse_da1_has_sixel(b"\x1b[?62;4;6;9;15;22c"));
        // foot: 4 present among others.
        assert!(parse_da1_has_sixel(b"\x1b[?62;4c"));
        // VT100-class, no sixel.
        assert!(!parse_da1_has_sixel(b"\x1b[?1;2c"));
        // "4" only as a substring of "14" must not match.
        assert!(!parse_da1_has_sixel(b"\x1b[?14;62c"));
        // Garbage / no terminator.
        assert!(!parse_da1_has_sixel(b"\x1b[?62;4"));
        assert!(!parse_da1_has_sixel(b"nonsense"));
    }

    #[test]
    fn test_unsupported_terminal_skips_display() {
        // Force the cached capability to "unsupported" and confirm a real image
        // path yields an informational message, not an error or a queued image.
        let _ = take_pending_sixel();
        set_sixel_supported(false);
        // Use a path that exists and is a regular file.
        let tmp = std::env::temp_dir().join("astra_sixel_unsupported_probe.png");
        std::fs::write(&tmp, b"not a real png").unwrap();
        let result = display_sixel(tmp.to_str().unwrap());
        assert!(!result.is_error);
        assert!(result.output.contains("does not support sixel"));
        assert!(take_pending_sixel().is_empty());
        let _ = std::fs::remove_file(&tmp);
        // Reset for other tests.
        SIXEL_CAP.store(0, Ordering::SeqCst);
    }

    #[test]
    fn test_set_tui_active_toggles() {
        set_tui_active(true);
        assert!(TUI_ACTIVE.load(Ordering::SeqCst));
        set_tui_active(false);
        assert!(!TUI_ACTIVE.load(Ordering::SeqCst));
    }
}
