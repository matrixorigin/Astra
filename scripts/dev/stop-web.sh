#!/bin/bash
# Stop web UI dev server.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"
# shellcheck source=scripts/dev/web-config.sh
source "$ROOT_DIR/scripts/dev/web-config.sh"

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

PORT="${ASTRA_WEB_PORT:-${WEB_PORT:-3536}}"

if [ -f "$PID_FILE" ]; then
    PID="$(cat "$PID_FILE")"
    rm -f "$PID_FILE"
    if kill -0 "$PID" 2>/dev/null; then
        if is_web_process "$PID"; then
            kill_and_wait "$PID"
            echo "✅ Web UI stopped"
        else
            echo "⚠️  PID $PID is not an Astra web UI process; leaving it untouched"
        fi
    else
        echo "✅ Web UI stopped"
    fi
fi

# Kill any remaining listeners on the web port (e.g. orphaned next-server children).
LISTENERS="$(web_port_listener_pids "$PORT")"
if [ -n "$LISTENERS" ]; then
    for p in $LISTENERS; do
        echo "⚠️  Killing leftover listener on port $PORT (PID: $p)"
        kill_and_wait "$p"
    done
fi

echo "✅ Web UI stopped"
