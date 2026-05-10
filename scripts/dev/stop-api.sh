#!/bin/bash
# Stop API server
# Note: uses kill on specific PIDs only — never kills process groups
# to avoid terminating parent make/shell processes.

PID_FILE="api_server.pid"

_is_astra_server() {
    local pid=$1
    local comm
    # /proc/$pid/comm is Linux-only (truncated to 15 chars; "astra-server" is
    # 12 so safe); falls back to ps(1) on macOS/BSD.
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || ps -p "$pid" -o comm= 2>/dev/null)
    [[ "$comm" == "astra-server" ]]
}

_kill_and_wait() {
    local pid=$1
    kill "$pid" 2>/dev/null || return
    for i in {1..8}; do
        sleep 1
        kill -0 "$pid" 2>/dev/null || return
    done
    kill -9 "$pid" 2>/dev/null || true
}

# Try PID file first — verify it's actually an astra-server process
if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    rm -f "$PID_FILE"
    if kill -0 "$PID" 2>/dev/null && _is_astra_server "$PID"; then
        _kill_and_wait "$PID"
    fi
fi

# Fallback: kill any remaining astra-server binary processes.
# Use pgrep -x to match the exact binary name (not shell scripts that
# reference "astra-server" in variables). This prevents accidentally
# killing make or other parent processes.
PIDS=$(pgrep -x "astra-server" 2>/dev/null)
if [ -n "$PIDS" ]; then
    echo "$PIDS" | xargs -r kill 2>/dev/null || true
    sleep 2
    pgrep -x "astra-server" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
fi

echo "✅ API server stopped"
