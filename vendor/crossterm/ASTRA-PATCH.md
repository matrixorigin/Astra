# Local Astra startup-query prototype

Base: crossterm 0.29.0, as published on crates.io (MIT; see LICENSE).
The package records upstream commit 36d95b26a26e64b0f8c12edfe11f410a6d56a812.
This directory is a local dependency patch, not a new upstream release.
The application must not depend on a modified Cargo registry cache.

## Review fixes

This dependency patch and the color/readability changes for PR #729 are developed
together on `investigate/issue-728-light-terminal`.

- SIGINT is registered before raw mode. Astra polls it during asynchronous
  startup and transfers the same listener to its TUI shutdown monitor. Terminal
  restoration happens in ordinary Rust cleanup, never under a signal handler.
- Drain reads check readiness even when stdin is blocking, so a partial response
  cannot bypass the parser/poll deadline. EOF and read errors are propagated.
- Esc lookahead is enabled only while query_startup_attributes owns the reader.
  Completing or failing the query disables lookahead and releases a pending Esc.
- Oversized and timed-out recognized responses quarantine their tails until BEL,
  ST, or a new Esc sequence. No unbounded buffer or expired tail becomes keys.
  A lone Esc recovers a missing terminator after up to 40 ms of ST lookahead.
- Astra detects palette or theme initialization before the query: it warns and
  debug-asserts instead of silently discarding colors.
- The shared query retains a 300 ms budget. Missing DA1 stays unknown; the reader
  caches late DA1 for Astra's event adapter. An active TUI never falls back to a
  second /dev/tty reader.

PTY regression coverage includes bare Esc through poll and EventStream, separate
Esc then character, arrows/Alt, startup and post-handoff SIGINT, late DA1,
oversized timeouts, missing terminators, and early palette/theme initialization.
The FIFO/error-preservation fixes below can be proposed upstream independently.

To inspect or re-export the patch against the published 0.29.0 sources, normalize
line endings or use `diff -ru --strip-trailing-cr <upstream> vendor/crossterm`.
Some edited files use LF while the original package contains CRLF files.

## Implementation

The extension adds query_startup_attributes to the existing Unix input reader:
OSC 10/11 and DA1 use one deadline and leave keyboard/paste events in FIFO order.
Late color replies stay internal instead of becoming keystrokes. The two Unix
backends share the existing keyboard parser and a bounded OSC framing layer.
During a startup query an ambiguous standalone Esc/Alt+] waits up to 40 ms.
Outside that query, a standalone Esc is delivered immediately, matching upstream;
a late reply split immediately after its first Esc is consequently ambiguous
with keyboard input and cannot be reconstructed. Once an OSC prefix is recognized,
its framing remains internal across reads. An unfinished recognized response
enters quarantine after 500 ms; response storage is limited to 256 bytes. Bracketed paste
contents are never interpreted as query responses. Querying is startup-only;
callers own terminal modes and must not run an EventStream concurrently.

Also fixes filtered-read FIFO order and preserving skipped events on input errors.
No public Event variants or Windows input behavior are changed.

Changed upstream files:
- Cargo.toml: local patch MSRV is 1.70; events uses filedescriptor for readiness
  checks without changing stdin's shared O_NONBLOCK flags.
- src/terminal/sys/unix.rs: remove redundant parentheses flagged by the pinned toolchain.
- src/event/sys/unix/waker/mio.rs, src/terminal.rs: fix an upstream lint-name
  typo and redundant formatting borrow for the pinned toolchain.
- src/event.rs: internal responses and startup query export.
- src/event/filter.rs: DA1 parameters.
- src/event/read.rs: filtered FIFO and error preservation, regression test.
- src/event/sys/unix/parse.rs: OSC framing and DA1 parameter decoding.
- src/event/source/unix.rs, unix/mio.rs, unix/tty.rs: shared bounded parser and deadlines.

Added files: src/event/startup_query.rs, src/event/source/unix/parser.rs.
Astra's PTY integration tests exercise the real query, theme, terminal guard and
EventStream without credentials or network requests. The patch's own tests run
with cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features event-stream.
The committed standalone Cargo.lock pins this unit-test lane independently of
the application's root lockfile. Updating test dependencies requires an
explicit lockfile update; these unit tests do not replace the real-reader PTY
tests below. CI runs those PTY tests on both Linux and macOS.

Shipping this prototype requires maintaining this fork until a released upstream
API provides the required query and input-preservation behavior. Do not silently
replace it with a library that reads /dev/tty independently of crossterm.

## Astra regression commands

```sh
cargo test --locked -p astra-cli --lib tui::terminal_startup -- --test-threads=2
cargo test --locked -p astra-cli --lib --features crossterm/use-dev-tty tui::terminal_startup -- --test-threads=2
```

These PTY tests use fake terminal replies and no network or credentials. Run both
backends when changing reader readiness, framing, deadlines, or query handoff.

The fragmented-color PTY fixture sends four chunks at absolute offsets, rather
than sleeping once per byte and accumulating scheduler delays. It still splits
the opening ESC, ST terminator, and RGB payload. The parser unit tests cover
every two-part split and byte-at-a-time replies directly, since PTY writes may
be coalesced by the OS. Product query and parser deadlines remain unchanged.
Failures include the decoded probe result and parent query/write/ready
timestamps (`pty_timing`, milliseconds since child spawn completed), alongside
the child's independent startup `elapsed_ms`, to distinguish missing/late
replies from incorrect parsing or theme selection. These are test diagnostics,
not application telemetry or user terminal content.

The Sixel/DA1 PTY case uses an 80 ms delayed reply: enough to prove that the
query continues after both colors arrive, while retaining substantial headroom
inside the real 300 ms product budget on a loaded hosted runner. The assertion
uses the recorded parent timestamps to prove that the delay actually occurred.
It intentionally does not use wall-clock scheduling to test behavior close to
the timeout boundary.
