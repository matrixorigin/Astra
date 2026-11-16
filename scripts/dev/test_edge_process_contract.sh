#!/usr/bin/env bash
# Verify that repo lifecycle commands distinguish owned and unrelated processes.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${script_dir}/edge-process.sh"

fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-edge-process.XXXXXX")"
owned_pid=""
unrelated_pid=""
cleanup() {
    [ -z "$owned_pid" ] || kill "$owned_pid" 2>/dev/null || true
    [ -z "$unrelated_pid" ] || kill "$unrelated_pid" 2>/dev/null || true
    rm -rf "$fixture_root"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

mkdir -p "${fixture_root}/target/debug"
mkdir -p "${fixture_root}/scripts/dev"
# Exercise the real scripts in an isolated checkout, without sourcing the
# developer's .env or allowing its PID/lock settings to affect the test.
cp "${script_dir}/edge-process.sh" "${script_dir}/stop-edge.sh" "${fixture_root}/scripts/dev/"
if [ "$(uname -s)" = "Darwin" ]; then
    # A relocated system binary can be killed even after ad-hoc signing.
    # Build a disposable process instead of changing host security settings.
    if ! command -v cc >/dev/null 2>&1; then
        echo "macOS edge-process fixture requires a C compiler (Command Line Tools)" >&2
        exit 1
    fi
    printf '#include <unistd.h>\nint main(void) { sleep(30); return 0; }\n' |
        cc -x c - -o "${fixture_root}/target/debug/astra-edge"
    "${fixture_root}/target/debug/astra-edge" 30 &
else
    # On multicall coreutils, a renamed copy of sleep dispatches by argv[0]
    # and exits because "astra-edge" is not an applet. Python is already
    # required by the repository validator and can run through a symlink while
    # retaining the exact argv[0] that the ownership check inspects.
    python_executable="$(python3 -c 'import os, sys; print(os.path.realpath(sys.executable))')"
    ln -s "$python_executable" "${fixture_root}/target/debug/astra-edge"
    "${fixture_root}/target/debug/astra-edge" -I -S -c 'import time; time.sleep(30)' &
fi
owned_pid=$!
/bin/sleep 30 &
unrelated_pid=$!

# The child can still be between fork and exec on a busy runner.
owned_ready=false
for _ in {1..100}; do
    if edge_process_is_owned "$fixture_root" "$owned_pid" 2>/dev/null; then
        owned_ready=true
        break
    fi
    kill -0 "$owned_pid" 2>/dev/null || break
    /bin/sleep 0.02
done
if [ "$owned_ready" != true ]; then
    echo "owned edge-process fixture did not start" >&2
    exit 1
fi
if edge_process_is_owned "$fixture_root" "$unrelated_pid"; then
    echo "ownership check accepted an unrelated process" >&2
    exit 1
fi
if edge_process_is_owned "$fixture_root" "not-a-pid"; then
    echo "ownership check accepted a malformed PID" >&2
    exit 1
fi

printf '%s\n' "$unrelated_pid" > "${fixture_root}/unrelated.pid"
ASTRA_EDGE_PID_FILE="${fixture_root}/unrelated.pid" \
ASTRA_EDGE_LOCK_DIR="${fixture_root}/edge.lock" \
    "${fixture_root}/scripts/dev/stop-edge.sh" >/dev/null
if ! kill -0 "$unrelated_pid" 2>/dev/null; then
    echo "stop-edge terminated a process not owned by this checkout" >&2
    exit 1
fi
