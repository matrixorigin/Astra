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
cp /bin/sleep "${fixture_root}/target/debug/astra-edge"
# macOS may reject a relocated platform binary with SIGKILL. Ad-hoc sign
# only the disposable fixture; do not change /bin/sleep or host security policy.
if [ "$(uname -s)" = "Darwin" ]; then
    if ! command -v codesign >/dev/null 2>&1; then
        echo "macOS edge-process fixture requires codesign (Command Line Tools)" >&2
        exit 1
    fi
    codesign --force --sign - "${fixture_root}/target/debug/astra-edge"
fi
"${fixture_root}/target/debug/astra-edge" 30 &
owned_pid=$!
/bin/sleep 30 &
unrelated_pid=$!

edge_process_is_owned "$fixture_root" "$owned_pid"
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
