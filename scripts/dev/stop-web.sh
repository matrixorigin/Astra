#!/bin/bash
# Stop web UI dev server.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

PID_FILE="${ASTRA_WEB_PID_FILE:-web_server.pid}"
if [[ "$PID_FILE" != /* ]]; then
    PID_FILE="$ROOT_DIR/$PID_FILE"
fi

is_web_process() {
    local pid=$1
    local cmd
    cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
    [[ "$cmd" == *"node"* && "$cmd" == *"next/dist/bin/next"* && "$cmd" == *" dev"* ]]
}

kill_and_wait() {
    local pid=$1
    kill "$pid" 2>/dev/null || return 0
    for _ in {1..8}; do
        sleep 1
        kill -0 "$pid" 2>/dev/null || return 0
    done
    kill -9 "$pid" 2>/dev/null || true
}

echo "Stopping web UI..."

if [ -f "$PID_FILE" ]; then
    PID="$(cat "$PID_FILE")"
    rm -f "$PID_FILE"
    if kill -0 "$PID" 2>/dev/null; then
        if is_web_process "$PID"; then
            kill_and_wait "$PID"
            echo "✅ Web UI stopped"
            exit 0
        fi
        echo "⚠️  PID $PID is not an Astra web UI process; leaving it untouched"
        exit 1
    fi
    echo "✅ Web UI stopped"
    exit 0
fi

echo "✅ Web UI stopped"
