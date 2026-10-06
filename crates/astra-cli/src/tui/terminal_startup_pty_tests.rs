use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::pty::{Winsize, openpty};
use serde_json::Value;

use crate::tui::terminal_startup::StartupTerminal;

const CHILD: &str = "tui::terminal_startup::pty_tests::probe_child";
const RESULT: &str = "ASTRA_PROBE_RESULT=";
const INPUT: &str = "a你\x1b[200~pasted\n你好\x1b]11;rgb:ff/ff/ff\x07\x1b[201~";
// Long enough to prove that receiving colors does not finish the query before
// DA1, while leaving ample headroom inside the real 300 ms product budget on
// loaded hosted runners. This wall-clock PTY fixture does not claim to verify
// behavior at the exact timeout boundary.
const DELAYED_DA1_WAIT: Duration = Duration::from_millis(80);

// Split after the opening ESC, inside the ST terminator, and inside the RGB
// payload. Do not sleep once per byte: scheduler delays can accumulate beyond
// the real 300 ms startup budget. Exhaustive byte boundaries are also covered
// directly by the vendored parser tests, without OS scheduling or PTY coalescing.
const LIGHT_RESPONSE_PARTS: [&[u8]; 4] = [
    b"\x1b",
    b"]10;rgb:0000/0000/0000\x1b",
    b"\\\x1b]11;rgb:ffff/",
    b"ffff/ffff\x07",
];

#[derive(Debug, Default, serde::Serialize)]
struct PtyTiming {
    // All parent timestamps are milliseconds since spawn completed; child
    // elapsed_ms measures StartupTerminal::begin independently.
    query_budget_ms: u128,
    query_seen_ms: Option<u128>,
    response_write_end_ms: Vec<u128>,
    da1_written_ms: Option<u128>,
    startup_ready_seen_ms: Option<u128>,
    input_ready_seen_ms: Option<u128>,
    resize_applied_ms: Option<u128>,
    cursor_query_seen_ms: Vec<u128>,
    cursor_response_write_end_ms: Vec<Option<u128>>,
}

fn terminal_modes_restored(
    before: &nix::sys::termios::Termios,
    after: &nix::sys::termios::Termios,
) -> bool {
    let before = libc::termios::from(before.clone());
    let after = libc::termios::from(after.clone());
    #[cfg(target_os = "macos")]
    let (before, after) = {
        // Darwin sets PENDIN when canonical input is restored, even for a
        // plain tcsetattr raw/restore pair. It is kernel pending-input state,
        // not a mode we configured. Compare all other flags, cc and speeds.
        let (mut before, mut after) = (before, after);
        before.c_lflag &= !libc::PENDIN;
        after.c_lflag &= !libc::PENDIN;
        (before, after)
    };
    before == after
}

/// Run the real startup guard, theme getter, terminal guard and EventStream
/// without initializing a session, accessing credentials or contacting a model.
#[test]
fn probe_child() {
    let Ok(case) = std::env::var("ASTRA_TEST_TERMINAL_PROBE") else {
        return;
    };
    if case == "non_tty" {
        let _guard = StartupTerminal::begin().unwrap();
        println!("NO_QUERY");
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let before = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
    let started = Instant::now();
    if case == "early_palette" || case == "early_theme" {
        if case == "early_palette" {
            let _ = crate::tui::terminal_palette::default_bg();
        } else {
            let _ = crate::tui::theme::current();
        }
        let result = std::panic::catch_unwind(StartupTerminal::begin);
        assert!(
            result.is_err(),
            "early initialization must not silently discard query results"
        );
        let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
        println!("STARTUP_READY");
        println!(
            "{RESULT}{}",
            serde_json::json!({"restored": terminal_modes_restored(&before, &after)})
        );
        return;
    }
    let mut startup = StartupTerminal::begin().unwrap();
    let elapsed_ms = started.elapsed().as_millis();
    let startup_mode = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
    assert_eq!(
        startup_mode.local_flags & nix::sys::termios::LocalFlags::ISIG,
        before.local_flags & nix::sys::termios::LocalFlags::ISIG,
        "startup must retain interrupt signals"
    );
    let bg = crate::tui::terminal_palette::default_bg();
    let fg = crate::tui::terminal_palette::default_fg();
    let theme = *crate::tui::theme::current();
    println!("STARTUP_READY");
    if matches!(case.as_str(), "sigint" | "sigint_query") {
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), startup.interrupted())
                .await
                .expect("startup SIGINT was not delivered");
        });
    }
    if matches!(case.as_str(), "abort" | "sigint" | "sigint_query") {
        drop(startup);
        let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
        println!(
            "{RESULT}{}",
            serde_json::json!({ "restored": terminal_modes_restored(&before, &after) })
        );
        return;
    }
    if case == "late" {
        // Replies arrive during ordinary startup output, before TUI ownership.
        std::thread::sleep(Duration::from_millis(100));
    }
    let sixel_before = astra_tools::display_sixel::cached_sixel_support();
    startup.prepare_tui().unwrap();
    if case == "keyboard_enhancement_panic" {
        // Exercises the P2 review finding on #914: Push/Pop is a stack, not
        // an idempotent toggle, so a panic that runs both the global panic
        // hook and TerminalGuard's own Drop during unwind must still only
        // remove Astra's own entry once. `_guard` deliberately lives inside
        // this closure, not outside it, so its Drop runs as part of the
        // unwind that catch_unwind stops here -- the same order a real
        // uncaught panic during the interactive session would hit.
        assert!(
            startup.keyboard_enhancement_supported(),
            "fixture must simulate a terminal that supports the protocol"
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard =
                crate::tui::terminal::TerminalGuard::init(startup.keyboard_enhancement_supported())
                    .unwrap();
            panic!("deliberate panic to exercise keyboard-enhancement cleanup ownership");
        }));
        assert!(result.is_err(), "fixture must actually panic");
        startup.handoff();
        drop(startup);
        let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
        println!(
            "{RESULT}{}",
            serde_json::json!({ "restored": terminal_modes_restored(&before, &after) })
        );
        return;
    }
    let mut guard =
        crate::tui::terminal::TerminalGuard::init(startup.keyboard_enhancement_supported())
            .unwrap();
    if case == "resize_missed" {
        // The shared reader is initialized by startup. Discard SIGWINCH to
        // exercise the size watchdog, without a synthetic resize event.
        unsafe {
            libc::signal(libc::SIGWINCH, libc::SIG_IGN);
        }
    }
    startup.handoff();
    if case == "late_da1" {
        assert!(sixel_before.is_none());
        let image = tempfile::NamedTempFile::new().unwrap();
        let result = astra_tools::display_sixel::display_sixel(image.path().to_str().unwrap());
        assert!(result.output.contains("has not been confirmed"));
        assert!(astra_tools::display_sixel::cached_sixel_support().is_none());
    }
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut stream = crate::tui::event::TuiEventStream::new(rx, guard.resize_pending.clone());
    println!("INPUT_READY");
    if case == "sigint_handoff" {
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(500), startup.interrupted())
                .await
                .expect("SIGINT listener was lost at handoff");
        });
        drop(guard);
        let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
        println!(
            "{RESULT}{}",
            serde_json::json!({"restored": terminal_modes_restored(&before, &after)})
        );
        return;
    }
    if case == "poll_escape" {
        assert!(crossterm::event::poll(Duration::from_millis(200)).unwrap());
        assert_eq!(
            crossterm::event::read().unwrap(),
            crossterm::event::Event::Key(crossterm::event::KeyCode::Esc.into())
        );
        drop(guard);
        let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
        println!(
            "{RESULT}{}",
            serde_json::json!({"restored": terminal_modes_restored(&before, &after)})
        );
        return;
    }
    let events = runtime.block_on(async {
        use tokio_stream::StreamExt;
        let mut events = Vec::new();
        let mut resize_observed = false;
        let budget = if matches!(
            case.as_str(),
            "escape" | "escape_then_f" | "arrow" | "alt" | "query_escape"
        ) {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(2)
        };
        let mut deadline = tokio::time::Instant::now() + budget;
        while events.len() < expected_events(&case).len() {
            let event = tokio::time::timeout_at(deadline, stream.next())
                .await
                .unwrap_or_else(|error| {
                    panic!("input was not preserved: {error:?}; collected={events:?}")
                })
                .expect("input stream ended");
            if let crate::tui::event::TuiEvent::Resize {
                cursor,
                size,
                interrupted,
            } = &event
            {
                guard
                    .reconcile_resize(*cursor, *size, *interrupted)
                    .unwrap();
            }
            let observed_before = events.len();
            match event {
                crate::tui::event::TuiEvent::Key(key) => {
                    events.push(format!("key:{:?}:{:?}", key.code, key.modifiers))
                }
                crate::tui::event::TuiEvent::Paste(text) => events.push(format!("paste:{text}")),
                crate::tui::event::TuiEvent::Resize {
                    interrupted: true, ..
                } if case == "resize_timeout" => {
                    tx.send(()).await.unwrap();
                    assert!(
                        matches!(
                            tokio::time::timeout(Duration::from_millis(50), stream.next()).await,
                            Ok(Some(crate::tui::event::TuiEvent::Draw))
                        ),
                        "cursor query blocked draw delivery"
                    );
                }
                crate::tui::event::TuiEvent::Resize {
                    cursor,
                    size,
                    interrupted: false,
                } if case.starts_with("resize_") => {
                    assert!(guard.terminal.viewport_area.y < size.1);
                    // One requested resize may produce OS notifications and
                    // watchdog retries. They must not stand in for input.
                    if !resize_observed {
                        resize_observed = true;
                        events.push(format!("resize:{cursor:?}"));
                    }
                }
                _ => {}
            }
            // Keep the original per-input budget, but retries are not
            // progress and cannot extend the wait for missing input.
            if events.len() > observed_before {
                deadline = tokio::time::Instant::now() + budget;
            }
        }
        // A late OSC reply must not become a fourth keyboard event.
        let extra_input = tokio::time::timeout(Duration::from_millis(40), async {
            loop {
                match stream.next().await {
                    Some(
                        event @ (crate::tui::event::TuiEvent::Key(_)
                        | crate::tui::event::TuiEvent::Paste(_)),
                    ) => break event,
                    None => panic!("input stream ended"),
                    _ => {} // Resize notifications may be duplicated/coalesced by the OS.
                }
            }
        })
        .await;
        assert!(
            extra_input.is_err(),
            "unexpected input {extra_input:?}; collected={events:?}"
        );
        events
    });
    drop(guard);
    drop(startup);
    let after = nix::sys::termios::tcgetattr(std::io::stdin()).unwrap();
    println!(
        "{RESULT}{}",
        serde_json::json!({
            "elapsed_ms": elapsed_ms,
            "sixel_before": sixel_before,
            "sixel_after": astra_tools::display_sixel::cached_sixel_support(),
            "bg": bg,
            "fg": fg,
            "light": theme.is_light,
            "plain": theme.accent == ratatui::style::Color::Reset,
            "events": events,
            "restored": terminal_modes_restored(&before, &after),
        })
    );
}

fn expected_events(case: &str) -> Vec<String> {
    match case {
        "escape" | "query_escape" | "truncated_escape" => {
            vec!["key:Esc:KeyModifiers(0x0)".to_string()]
        }
        "escape_then_f" => vec![
            "key:Esc:KeyModifiers(0x0)".to_string(),
            "key:Char('f'):KeyModifiers(0x0)".to_string(),
        ],
        "resize_reply" | "resize_fragmented" | "resize_timeout" | "resize_missed"
        | "resize_invalid" => vec![
            if case == "resize_invalid" {
                "resize:Some((65534, 65534))".to_string()
            } else if case != "resize_timeout" {
                "resize:Some((2, 4))".to_string()
            } else {
                "resize:None".to_string()
            },
            "key:Char('a'):KeyModifiers(0x0)".to_string(),
            "paste:resize paste".to_string(),
        ],
        "arrow" => vec!["key:Up:KeyModifiers(0x0)".to_string()],
        "alt" => vec!["key:Char('f'):KeyModifiers(ALT)".to_string()],
        _ => vec![
            "key:Char('a'):KeyModifiers(0x0)".to_string(),
            "key:Char('你'):KeyModifiers(0x0)".to_string(),
            "paste:pasted\n你好\x1b]11;rgb:ff/ff/ff\x07".to_string(),
        ],
    }
}

fn special_input(case: &str) -> bool {
    matches!(
        case,
        "escape"
            | "resize_reply"
            | "resize_fragmented"
            | "resize_timeout"
            | "resize_missed"
            | "resize_invalid"
            | "poll_escape"
            | "escape_then_f"
            | "arrow"
            | "alt"
            | "query_escape"
            | "oversized"
            | "truncated_escape"
            | "late_da1"
            | "sigint"
            | "sigint_query"
            | "sigint_handoff"
    )
}

fn run_case(case: &str) -> (Value, Vec<u8>) {
    let pty = openpty(
        Some(&Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let mut master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--nocapture"])
        .env("ASTRA_TEST_TERMINAL_PROBE", case)
        .env("ASTRA_TUI_THEME", "auto")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove("NO_COLOR")
        .env_remove("ASTRA_TERMINAL_FG")
        .env_remove("ASTRA_TERMINAL_BG")
        .env_remove("COLORFGBG")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    match case {
        "light" | "fragmented" => {
            command.env("COLORFGBG", "15;0");
        }
        "dark" | "malformed" => {
            command.env("COLORFGBG", "0;15");
        }
        "explicit" | "early_theme" => {
            command.env("ASTRA_TUI_THEME", "dark");
        }
        "no_color" => {
            command.env("NO_COLOR", "1");
        }
        "background_override" => {
            command.env("ASTRA_TERMINAL_BG", "#ffffff");
        }
        _ => {}
    }
    // SAFETY: the child only establishes its controlling PTY before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    let parent_started = Instant::now();
    let deadline = parent_started + Duration::from_secs(10);
    let mut timing = PtyTiming {
        query_budget_ms: super::QUERY_TIMEOUT.as_millis(),
        ..PtyTiming::default()
    };
    let mut output = Vec::new();
    let mut replied = false;
    let mut sent_input = false;
    let mut cursor_replies = 0;
    loop {
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            panic!(
                "PTY case {case} timed out; timing={timing:?}: {}",
                String::from_utf8_lossy(&output)
            );
        }
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one valid pollfd for the owned PTY.
        if unsafe { libc::poll(&mut descriptor, 1, 50) } > 0 {
            let mut chunk = [0; 4096];
            match master.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => output.extend_from_slice(&chunk[..count]),
            }
        }
        if timing.startup_ready_seen_ms.is_none()
            && output.windows(13).any(|bytes| bytes == b"STARTUP_READY")
        {
            timing.startup_ready_seen_ms = Some(parent_started.elapsed().as_millis());
        }
        if timing.input_ready_seen_ms.is_none()
            && output.windows(11).any(|bytes| bytes == b"INPUT_READY")
        {
            timing.input_ready_seen_ms = Some(parent_started.elapsed().as_millis());
        }
        if !replied && output.windows(3).any(|bytes| bytes == b"\x1b[c") {
            replied = true;
            timing.query_seen_ms = Some(parent_started.elapsed().as_millis());
            if !matches!(case, "late" | "unsupported") && !special_input(case) {
                master.write_all(INPUT.as_bytes()).unwrap();
                sent_input = true;
            }
            if case == "query_escape" {
                master.write_all(b"\x1b").unwrap();
                sent_input = true;
            }
            if case == "sigint_query" {
                master.write_all(b"\x03").unwrap();
                sent_input = true;
            }
            let response = match case {
                "light" | "fragmented" | "delayed_da1" => {
                    "\x1b]10;rgb:0000/0000/0000\x1b\\\x1b]11;rgb:ffff/ffff/ffff\x07"
                }
                "dark" => "\x1b]11;rgb:0000/0000/0000\x1b\\\x1b]10;rgb:eeee/eeee/eeee\x07",
                "background_only" => "\x1b]11;rgb:ffff/ffff/ffff\x07",
                "malformed" => "\x1b]10;#你abc\x07\x1b]11;rgb:bad/nope/ff\x1b\\",
                _ => "",
            };
            if case == "fragmented" {
                assert_eq!(LIGHT_RESPONSE_PARTS.concat(), response.as_bytes());
                let send_started = Instant::now();
                for (index, part) in LIGHT_RESPONSE_PARTS.iter().enumerate() {
                    // Absolute deadlines avoid accumulating timer overshoot.
                    let due = send_started + Duration::from_millis(2 * index as u64);
                    let remaining = due.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        std::thread::sleep(remaining);
                    }
                    master.write_all(part).unwrap();
                    timing
                        .response_write_end_ms
                        .push(parent_started.elapsed().as_millis());
                }
            } else {
                master.write_all(response.as_bytes()).unwrap();
                timing
                    .response_write_end_ms
                    .push(parent_started.elapsed().as_millis());
            }
            if !matches!(
                case,
                "unsupported" | "query_escape" | "sigint_query" | "late_da1"
            ) {
                if case == "delayed_da1" {
                    let due = Instant::now() + DELAYED_DA1_WAIT;
                    let remaining = due.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        std::thread::sleep(remaining);
                    }
                }
                if case == "keyboard_enhancement_panic" {
                    // Answers CSI ?u before DA1, exactly like a real
                    // supporting terminal per the Kitty protocol's own
                    // recommended detection order -- flags=0 (no
                    // enhancement currently active from any parent
                    // program), which is still `Some(..)`, not `None`.
                    master.write_all(b"\x1b[?0u").unwrap();
                }
                let da1 = if case == "no_sixel" {
                    b"\x1b[?1;2c".as_slice()
                } else {
                    b"\x1b[?62;4;6c"
                };
                master.write_all(da1).unwrap();
                timing.da1_written_ms = Some(parent_started.elapsed().as_millis());
            }
        }
        if !sent_input
            && !special_input(case)
            && output.windows(13).any(|bytes| bytes == b"STARTUP_READY")
        {
            if case == "late" {
                master
                    .write_all(b"\x1b]10;rgb:00/00/00\x07\x1b]11;rgb:ff/ff/ff\x1b\\")
                    .unwrap();
            }
            master.write_all(INPUT.as_bytes()).unwrap();
            sent_input = true;
        }
        if case == "sigint" && !sent_input && output.windows(13).any(|b| b == b"STARTUP_READY") {
            master.write_all(b"\x03").unwrap();
            sent_input = true;
        }
        if special_input(case) && !sent_input && output.windows(11).any(|b| b == b"INPUT_READY") {
            sent_input = true;
            match case {
                "escape" | "poll_escape" => master.write_all(b"\x1b").unwrap(),
                "escape_then_f" => {
                    master.write_all(b"\x1b").unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                    master.write_all(b"f").unwrap();
                }
                "resize_reply" | "resize_fragmented" | "resize_timeout" | "resize_missed"
                | "resize_invalid" => {
                    if case == "resize_missed" {
                        // Let the idle reader consume an unchanged watchdog
                        // tick before resizing without a SIGWINCH wake.
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    let size = Winsize {
                        ws_row: 20,
                        ws_col: 60,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    // SAFETY: valid PTY and window-size pointer.
                    assert_eq!(
                        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                        0
                    );
                    timing.resize_applied_ms = Some(parent_started.elapsed().as_millis());
                }
                "arrow" => master.write_all(b"\x1b[A").unwrap(),
                "alt" => master.write_all(b"\x1bf").unwrap(),
                // SAFETY: this is the live test child we just spawned.
                "sigint_handoff" => unsafe {
                    libc::kill(child.id() as i32, libc::SIGINT);
                },
                "truncated_escape" => master.write_all(b"\x1b]11;\x1b").unwrap(),
                "oversized" => {
                    master.write_all(b"\x1b]11;").unwrap();
                    master.write_all(&[b'x'; 300]).unwrap();
                    std::thread::sleep(Duration::from_millis(550));
                    master.write_all(b"xyz\x07").unwrap();
                    master.write_all(INPUT.as_bytes()).unwrap();
                }
                "late_da1" => {
                    master.write_all(b"\x1b[?62;4;6c").unwrap();
                    master.write_all(INPUT.as_bytes()).unwrap();
                }
                _ => unreachable!(),
            }
        }
        let queries = output
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[6n")
            .count();
        if queries > cursor_replies {
            timing
                .cursor_query_seen_ms
                .push(parent_started.elapsed().as_millis());
            cursor_replies += 1;
            if cursor_replies == 1 {
                master.write_all(b"\x1b[8;1R").unwrap();
            } else if case.starts_with("resize_") {
                // User input received during DSR must survive in FIFO order.
                if cursor_replies == 2 {
                    master.write_all(b"a").unwrap();
                }
                if case != "resize_timeout" {
                    if case == "resize_fragmented" {
                        // Split inside a recognized CSI, not after a bare ESC:
                        // hosted-runner scheduling can exceed the 40 ms Escape
                        // ambiguity window even for a requested 5 ms sleep.
                        // Parser unit tests cover every CPR byte boundary.
                        master.write_all(b"\x1b[5;").unwrap();
                        master.write_all(b"3R").unwrap();
                    } else if case == "resize_invalid" {
                        master.write_all(b"\x1b[65535;65535R").unwrap();
                    } else {
                        master.write_all(b"\x1b[5;3R").unwrap();
                    }
                }
                if cursor_replies == 2 {
                    master.write_all(b"\x1b[200~resize paste\x1b[201~").unwrap();
                }
            }
            timing.cursor_response_write_end_ms.push(
                if cursor_replies == 1 || (case.starts_with("resize_") && case != "resize_timeout")
                {
                    Some(parent_started.elapsed().as_millis())
                } else {
                    None
                },
            );
        }
        if child.try_wait().unwrap().is_some() {
            // Read remaining output on the next poll; the slave closes at exit.
            continue;
        }
    }
    let status = child.wait().unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(
        status.success(),
        "PTY case {case} failed; timing={timing:?}: {text}"
    );
    assert!(
        text.contains("STARTUP_READY\r\n"),
        "startup newline handling: {text}"
    );
    let result = text
        .lines()
        .find_map(|line| line.split_once(RESULT).map(|(_, value)| value))
        .unwrap_or_else(|| panic!("no result for {case}; timing={timing:?}: {text}"));
    let mut value: Value = serde_json::from_str(result).unwrap();
    value["pty_timing"] = serde_json::to_value(timing).unwrap();
    assert_eq!(value["restored"], true, "{case}: {value}");
    if matches!(
        case,
        "abort"
            | "sigint"
            | "sigint_query"
            | "sigint_handoff"
            | "poll_escape"
            | "early_palette"
            | "early_theme"
            | "keyboard_enhancement_panic"
    ) {
        return (value, output);
    }
    assert_eq!(
        value["events"],
        serde_json::json!(expected_events(case)),
        "{case}: {value}"
    );
    (value, output)
}

#[test]
fn pty_detects_light_dark_and_fragmented_responses() {
    for case in ["light", "fragmented", "background_only", "dark"] {
        let (value, _) = run_case(case);
        assert_eq!(value["light"], case != "dark", "{case}: {value}");
        assert_eq!(value["plain"], false, "{case}: {value}");
        if case == "fragmented" {
            assert_eq!(
                value["pty_timing"]["response_write_end_ms"]
                    .as_array()
                    .unwrap()
                    .len(),
                LIGHT_RESPONSE_PARTS.len(),
                "{case}: {value}"
            );
        }
    }
}

#[test]
fn pty_timeout_and_late_responses_preserve_input_and_cached_fallback() {
    for case in ["unsupported", "late"] {
        let (value, output) = run_case(case);
        assert!(value["bg"].is_null(), "{case}");
        let elapsed = value["elapsed_ms"].as_u64().unwrap();
        assert!((100..1000).contains(&elapsed), "{case}: {elapsed}");
        // No terminal echo of the response during startup (the JSON result
        // intentionally contains a pasted OSC, so inspect only the prefix).
        let prefix = String::from_utf8_lossy(&output);
        let prefix = prefix.split(RESULT).next().unwrap();
        assert!(!prefix.contains("rgb:"), "{case}: {prefix}");
    }
}

#[test]
fn pty_honors_manual_modes_and_background_override() {
    for case in ["explicit", "no_color", "background_override"] {
        let (value, output) = run_case(case);
        assert!(
            !output.windows(5).any(|bytes| bytes == b"\x1b]10;"),
            "{case}"
        );
        assert!(
            !output.windows(5).any(|bytes| bytes == b"\x1b]11;"),
            "{case}"
        );
        assert_eq!(value["plain"], case == "no_color", "{case}");
        assert_eq!(value["light"], case == "background_override", "{case}");
    }
}

#[test]
fn pty_malformed_colors_fall_back_without_panicking_or_leaking_keys() {
    let (value, _) = run_case("malformed");
    assert_eq!(value["light"], true);
}

#[test]
fn redirected_io_does_not_emit_terminal_queries() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", CHILD, "--nocapture"])
        .env("ASTRA_TEST_TERMINAL_PROBE", "non_tty")
        .env("ASTRA_TUI_THEME", "auto")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.contains(&27));
    assert!(String::from_utf8_lossy(&output.stdout).contains("NO_QUERY"));
}

#[test]
fn pty_aborted_startup_restores_terminal_modes() {
    run_case("abort");
}

#[test]
fn pty_escape_poll_stream_and_key_sequences() {
    for case in [
        "poll_escape",
        "escape",
        "escape_then_f",
        "arrow",
        "alt",
        "query_escape",
    ] {
        run_case(case);
    }
}

#[test]
fn pty_response_recovery_preserves_input() {
    for case in ["oversized", "truncated_escape"] {
        run_case(case);
    }
}

#[test]
fn pty_sigint_restores_terminal_and_survives_handoff() {
    for case in ["sigint", "sigint_query", "sigint_handoff"] {
        run_case(case);
    }
}

#[test]
fn pty_late_da1_is_unknown_until_reply() {
    let (value, output) = run_case("late_da1");
    assert_eq!(
        output
            .windows(3)
            .filter(|bytes| *bytes == b"\x1b[c")
            .count(),
        1,
        "unknown Sixel support must not start a competing query"
    );
    assert!(value["sixel_before"].is_null());
    assert_eq!(value["sixel_after"], true);
}

/// Regression for the P2 review finding on #914: Push/Pop keyboard
/// enhancement is a stack, not an idempotent toggle like bracketed paste.
/// A panic after a successful init runs both the global panic hook and
/// TerminalGuard's own Drop during unwind; only one of them may actually
/// consume the still-outstanding push. Popping twice would, on a terminal
/// where some parent program already had its own entry pushed, remove that
/// parent's entry instead of harmlessly no-op'ing.
#[test]
fn pty_keyboard_enhancement_push_and_pop_survive_a_panic_exactly_once() {
    let (_value, output) = run_case("keyboard_enhancement_panic");
    let count = |needle: &[u8]| {
        output
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    };
    assert_eq!(
        count(b"\x1b[?u"),
        1,
        "must query keyboard enhancement exactly once: {}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        count(b"\x1b[>1u"),
        1,
        "must push exactly once for a supported, detected terminal: {}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        count(b"\x1b[<1u"),
        1,
        "the panic hook and TerminalGuard::drop must not both pop the same \
         outstanding push -- that would remove a pre-existing parent \
         program's own keyboard mode instead of a no-op: {}",
        String::from_utf8_lossy(&output)
    );
}

#[cfg(debug_assertions)]
#[test]
fn pty_early_palette_or_theme_access_is_detected_and_restores_terminal() {
    for case in ["early_palette", "early_theme"] {
        run_case(case);
    }
}

#[test]
fn pty_sixel_waits_for_da1_and_records_negative_evidence() {
    let (value, _) = run_case("delayed_da1");
    assert_eq!(value["sixel_before"], true, "delayed_da1: {value}");
    let query_seen = value["pty_timing"]["query_seen_ms"].as_u64().unwrap();
    let da1_written = value["pty_timing"]["da1_written_ms"].as_u64().unwrap();
    assert!(
        da1_written.saturating_sub(query_seen) >= DELAYED_DA1_WAIT.as_millis() as u64,
        "DA1 was not observably delayed: {value}"
    );
    let (value, _) = run_case("no_sixel");
    assert_eq!(value["sixel_before"], false, "no_sixel: {value}");
}

#[test]
fn pty_resize_cursor_query_preserves_input_and_handles_missing_reply() {
    for case in [
        "resize_reply",
        "resize_fragmented",
        "resize_timeout",
        "resize_missed",
        "resize_invalid",
    ] {
        run_case(case);
    }
}
