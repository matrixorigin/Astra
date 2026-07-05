#!/bin/bash
# Stop local astra-edge provider.

set -euo pipefail

PID_FILE="${ASTRA_EDGE_PID_FILE:-astra_edge.pid}"

is_astra_edge() {
    local pid=$1
    local comm
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || ps -p "$pid" -o comm= 2>/dev/null || true)
    [[ "$comm" == "astra-edge" ]]
}

kill_and_wait() {
    local pid=$1
    kill "$pid" 2>/dev/null || return
    for _ in {1..8}; do
        sleep 1
        kill -0 "$pid" 2>/dev/null || return
    done
    kill -9 "$pid" 2>/dev/null || true
}

if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    rm -f "$PID_FILE"
    if kill -0 "$PID" 2>/dev/null && is_astra_edge "$PID"; then
        kill_and_wait "$PID"
    fi
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
