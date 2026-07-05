#!/bin/bash
# Stop local astra-edge provider.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

if [ -f "$REPO_ROOT/.env" ]; then
    set -a
    # shellcheck disable=SC1091
    source "$REPO_ROOT/.env"
    set +a
fi

PID_FILE="${ASTRA_EDGE_PID_FILE:-$REPO_ROOT/astra_edge.pid}"
LOCK_DIR="${ASTRA_EDGE_LOCK_DIR:-$REPO_ROOT/.astra_edge.lock}"
LOCK_ACQUIRED=0

if [ "${ASTRA_EDGE_LOCK_HELD:-0}" != "1" ]; then
    if ! mkdir "$LOCK_DIR" 2>/dev/null; then
        echo "⚠️  astra-edge start/stop is already in progress"
        exit 1
    fi
    LOCK_ACQUIRED=1
fi
trap 'if [ "$LOCK_ACQUIRED" = "1" ]; then rmdir "$LOCK_DIR" 2>/dev/null || true; fi' EXIT

is_astra_edge() {
    local pid=$1
    local comm
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || ps -p "$pid" -o comm= 2>/dev/null || true)
    [[ "$comm" == "astra-edge" ]]
}

kill_and_wait() {
    local pid=$1
    is_astra_edge "$pid" || return
    kill "$pid" 2>/dev/null || return
    for _ in {1..8}; do
        sleep 1
        kill -0 "$pid" 2>/dev/null || return
    done
    kill -9 "$pid" 2>/dev/null || true
}

if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    if kill -0 "$PID" 2>/dev/null && is_astra_edge "$PID"; then
        kill_and_wait "$PID"
    fi
    rm -f "$PID_FILE"
fi

PIDS=$(pgrep -x "astra-edge" 2>/dev/null || true)
if [ -n "$PIDS" ]; then
    for pid in $PIDS; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 2
    for pid in $(pgrep -x "astra-edge" 2>/dev/null || true); do
        kill -9 "$pid" 2>/dev/null || true
    done
fi

echo "✅ astra-edge stopped"
