#!/usr/bin/env bash
# Canonical test commands shared by the mini bundle and hosted fallback.
set -euo pipefail
lane="${1:?lane required}"
lane_workspace="$PWD"
observe_git() {
  python3 - "$lane_workspace" <<'PYTHON'
import os, subprocess, sys, time
started = time.monotonic()
subprocess.run(["/usr/bin/git", "-C", sys.argv[1], "-c", "core.fsmonitor=false",
                "-c", "core.untrackedCache=false", "rev-parse", "--show-toplevel"],
    env={"PATH": "/usr/bin:/bin", "HOME": "/nonexistent", "LANG": "C", "LC_ALL": "C",
         "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
         "GIT_CONFIG_SYSTEM": os.devnull, "GIT_OPTIONAL_LOCKS": "0",
         "GIT_TERMINAL_PROMPT": "0"}, check=True, timeout=60)
print(f"System Git observation initialized in {time.monotonic() - started:.3f}s")
PYTHON
}
case "$lane" in
  default-reader)
    cargo test --locked -p astra-cli --lib tui::terminal_startup -- --test-threads=2
    if [ "$(uname -s)" = Darwin ]; then
      observe_git
      cargo test --locked --manifest-path Cargo.toml -p astra-cli \
        --lib edge_tools::shell::tests::bash_echo_returns_output -- --exact
    fi
    ;;
  reflow)
    cargo build --locked -p astra-cli --bin astra
    npm ci --ignore-scripts --prefix scripts/tui-reflow
    ASTRA_TEST_BINARY="${CARGO_TARGET_DIR:-$PWD/target}/debug/astra" npm test --prefix scripts/tui-reflow
    ;;
  coordination)
    cargo clippy --locked --manifest-path Cargo.toml -p astra-tools --all-targets -- -D warnings
    observe_git
    cargo test --locked --manifest-path Cargo.toml -p astra-tools --lib \
      workspace_observation::tests:: -- --test-threads=1
    ;;
  dev-tty-reader)
    cargo test --locked -p astra-cli --lib --features crossterm/use-dev-tty tui::terminal_startup -- --test-threads=2
    ;;
  vendored)
    cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features event-stream
    cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features event-stream,use-dev-tty
    ;;
  native-mio)
    native_test=event::source::unix::mio::tests::cursor_input_does_not_discard_a_batched_reader_wake
    native_command=(cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features event-stream "$native_test")
    native_listing="$("${native_command[@]}" -- --exact --list)"
    native_count="$(printf '%s\n' "$native_listing" | awk -v expected="$native_test: test" '$0 == expected { count++ } END { print count+0 }')"
    if [[ "$native_count" != 1 ]]; then
      echo "Native Mio regression must be discovered exactly once (found $native_count)." >&2
      exit 1
    fi
    "${native_command[@]}" -- --exact
    ;;
  all)
    if [[ "$(uname -s)" == Darwin ]]; then
      # Linux owns full renderer/alternate-backend combinations. Keep the
      # native OS boundaries without rebuilding two extra Astra CLI graphs.
      bash "$0" default-reader
      bash "$0" coordination
      bash "$0" native-mio
      exit 0
    fi
    bash "$0" default-reader
    bash "$0" reflow
    bash "$0" coordination
    bash "$0" vendored
    bash "$0" dev-tty-reader
    ;;
  *) echo "Unknown macOS lane" >&2; exit 2 ;;
esac
