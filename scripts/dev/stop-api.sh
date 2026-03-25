#!/bin/bash
# Stop API server

PID_FILE="api_server.pid"

_kill_and_wait() {
    local pid=$1
    kill "$pid" 2>/dev/null || return
    # Wait up to 8 seconds for graceful shutdown
    for i in {1..8}; do
        sleep 1
        kill -0 "$pid" 2>/dev/null || return  # process gone
    done
    # Force kill if still alive
    kill -9 "$pid" 2>/dev/null || true
}

# Try PID file first
if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    rm -f "$PID_FILE"
    if kill -0 "$PID" 2>/dev/null; then
        _kill_and_wait "$PID"
    fi
fi

# Fallback: kill any remaining Rust api-shell processes
PIDS=$(pgrep -f "mo-agent-server" 2>/dev/null)
if [ -n "$PIDS" ]; then
    echo "$PIDS" | xargs -r kill 2>/dev/null || true
    sleep 2
    # Force kill stragglers
    pgrep -f "mo-agent-server" | xargs -r kill -9 2>/dev/null || true
fi

echo "✅ API server stopped"
