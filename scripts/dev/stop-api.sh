#!/bin/bash
# Stop API server
# Note: uses kill on specific PIDs only — never kills process groups
# to avoid terminating parent make/shell processes.

PID_FILE="api_server.pid"
REPO_ROOT="$(pwd -P)"
STOPPED=0
ENV_FILE="${ASTRA_ENV_FILE:-.env}"
if [ -f "$ENV_FILE" ]; then
    set -a
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    set +a
fi
API_PORT="${ASTRA_API_PORT:-17001}"

_is_astra_server() {
    local pid=$1
    local comm
    # /proc/$pid/comm is Linux-only (truncated to 15 chars; "astra-server" is
    # 12 so safe); falls back to ps(1) on macOS/BSD.
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || ps -p "$pid" -o comm= 2>/dev/null)
    [[ "$comm" == "astra-server" ]]
}

_is_current_checkout() {
    local pid=$1
    local cwd
    cwd=$(readlink -f "/proc/$pid/cwd" 2>/dev/null || true)
    if [ -z "$cwd" ] && command -v lsof >/dev/null 2>&1; then
        cwd=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
    fi
    [ -n "$cwd" ] && [ "$cwd" = "$REPO_ROOT" ]
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
    if kill -0 "$PID" 2>/dev/null && _is_astra_server "$PID" && _is_current_checkout "$PID"; then
        _kill_and_wait "$PID"
        STOPPED=1
    elif kill -0 "$PID" 2>/dev/null && _is_astra_server "$PID"; then
        echo "⚠️  Left API server PID $PID running: it belongs to another checkout or its owner cannot be proven"
    fi
fi

# If the launcher lost its PID file, recover only the Server listening on this
# checkout's configured port and only when its process cwd is this checkout.
# Never use a global `pgrep astra-server` fallback: two worktrees commonly run
# independent local Servers and one stop command must not kill both.
PIDS=""
LISTENERS=""
if command -v lsof >/dev/null 2>&1; then
    LISTENERS=$(lsof -nP -tiTCP:"$API_PORT" -sTCP:LISTEN 2>/dev/null || true)
    for pid in $LISTENERS; do
        if _is_astra_server "$pid" && _is_current_checkout "$pid"; then
            PIDS="${PIDS:+$PIDS }$pid"
        fi
    done
fi
if [ -n "$PIDS" ]; then
    for pid in $PIDS; do
        _kill_and_wait "$pid"
        STOPPED=1
    done
elif [ -n "$LISTENERS" ]; then
    echo "⚠️  Left API listener on port $API_PORT: it belongs to another checkout or process"
fi

if [ "$STOPPED" -eq 1 ]; then
    echo "✅ API server stopped"
else
    echo "ℹ️  No API server owned by this checkout was stopped"
fi
