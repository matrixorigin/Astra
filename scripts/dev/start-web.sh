#!/bin/bash
# Start web UI dev server.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"
# shellcheck source=scripts/dev/web-config.sh
source "$ROOT_DIR/scripts/dev/web-config.sh"

WEB_DIR="$ROOT_DIR/web"
PID_FILE="${ASTRA_WEB_PID_FILE:-web_server.pid}"
LOG_FILE="${ASTRA_WEB_LOG_FILE:-web_server.log}"
WEB_PORT="$(web_agent_port)"
WEB_HOST="$(web_agent_host)"
API_URL="${ASTRA_API_URL:-http://localhost:${ASTRA_API_PORT:-17001}}"
NEXT_DIST_DIR="${ASTRA_NEXT_DIST_DIR:-.next-dev}"
NEXT_BIN="$WEB_DIR/node_modules/next/dist/bin/next"

web_validate_port "$WEB_PORT"

if [[ "$PID_FILE" != /* ]]; then
    PID_FILE="$ROOT_DIR/$PID_FILE"
fi
if [[ "$LOG_FILE" != /* ]]; then
    LOG_FILE="$ROOT_DIR/$LOG_FILE"
fi

echo "Starting web UI (Next.js dev server)..."

is_web_process() {
    local pid=$1
    local cmd
    cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
    [[ "$cmd" == *"node"* && "$cmd" == *"next/dist/bin/next"* && "$cmd" == *" dev"* ]]
}

web_parent_process_for_port() {
    local listener ppid
    for listener in $(web_port_listener_pids "$WEB_PORT"); do
        ppid=$(ps -p "$listener" -o ppid= 2>/dev/null | tr -d '[:space:]' || true)
        if [ -n "$ppid" ] && is_web_process "$ppid"; then
            printf '%s\n' "$ppid"
            return 0
        fi
    done
    return 1
}

start_detached() {
    if command -v setsid >/dev/null 2>&1; then
        (
            cd "$WEB_DIR"
            setsid "$@" >> "$LOG_FILE" 2>&1 &
            echo $!
        ) > "$PID_FILE.tmp"
        DETACHED_PID="$(cat "$PID_FILE.tmp")"
        rm -f "$PID_FILE.tmp"
    elif command -v screen >/dev/null 2>&1; then
        # macOS does not ship setsid(1). A plain background nohup process can
        # still be tied to the launching process group in agent/IDE shells, so
        # use screen when available to create an actually detached web server.
        screen -dmS "astra-web-$WEB_PORT" bash -lc \
            'cd "$1"; log_file="$2"; shift 2; exec "$@" >> "$log_file" 2>&1' \
            bash "$WEB_DIR" "$LOG_FILE" "$@"
        DETACHED_PID=""
        for _ in {1..40}; do
            DETACHED_PID="$(web_parent_process_for_port || true)"
            if [ -n "$DETACHED_PID" ] && kill -0 "$DETACHED_PID" 2>/dev/null; then
                return
            fi
            sleep 0.25
        done
    else
        (
            cd "$WEB_DIR"
            nohup "$@" >> "$LOG_FILE" 2>&1 &
            echo $!
        ) > "$PID_FILE.tmp"
        DETACHED_PID="$(cat "$PID_FILE.tmp")"
        rm -f "$PID_FILE.tmp"
    fi
}

if [ -f "$PID_FILE" ]; then
    PID="$(cat "$PID_FILE")"
    if kill -0 "$PID" 2>/dev/null && is_web_process "$PID"; then
        RUNNING_PORT="$(web_next_dev_process_port "$PID")"
        if [ -n "$RUNNING_PORT" ] && [ "$RUNNING_PORT" != "$WEB_PORT" ]; then
            echo "❌ Web UI is already running on port $RUNNING_PORT (PID: $PID)"
            echo "   Stop it with make dev-web-stop before starting port $WEB_PORT."
            exit 1
        fi
        echo "⚠️  Web UI already running (PID: $PID, URL: http://localhost:$WEB_PORT)"
        exit 0
    fi
    rm -f "$PID_FILE"
fi

if ! command -v node >/dev/null 2>&1; then
    echo "❌ node is required to start the web UI"
    exit 1
fi

if [ ! -f "$NEXT_BIN" ]; then
    echo "❌ Web dependencies are not installed"
    echo "   Run: make dev-web-deps"
    exit 1
fi

LISTENERS="$(web_port_listener_pids "$WEB_PORT")"
if [ -n "$LISTENERS" ]; then
    echo "❌ Port $WEB_PORT is already in use by PID(s): $LISTENERS"
    echo "   Stop the existing web agent process before starting a new one."
    exit 1
fi

if web_port_accepts_connections "$WEB_PORT"; then
    echo "❌ Port $WEB_PORT is already accepting connections"
    echo "   Stop the existing web agent process before starting a new one."
    exit 1
fi

mkdir -p "$(dirname "$PID_FILE")" "$(dirname "$LOG_FILE")"
rm -f "$PID_FILE"

for webpack_cache_dir in \
    "$WEB_DIR/$NEXT_DIST_DIR/cache/webpack" \
    "$WEB_DIR/$NEXT_DIST_DIR/dev/cache/webpack"
do
    if [ -d "$webpack_cache_dir" ]; then
        echo "Clearing stale Next.js webpack dev cache: $webpack_cache_dir"
        rm -rf "$webpack_cache_dir"
    fi
done

{
    echo ""
    echo "=== astra web dev start $(date) ==="
    echo "WEB_HOST=$WEB_HOST"
    echo "WEB_PORT=$WEB_PORT"
    echo "ASTRA_API_URL=$API_URL"
    echo "ASTRA_NEXT_DIST_DIR=$NEXT_DIST_DIR"
} >> "$LOG_FILE"

start_detached env \
    ASTRA_API_URL="$API_URL" \
    ASTRA_NEXT_DIST_DIR="$NEXT_DIST_DIR" \
    PORT="$WEB_PORT" \
    node "$NEXT_BIN" dev --turbopack --hostname "$WEB_HOST" --port "$WEB_PORT"
PID="$DETACHED_PID"
if [ -z "$PID" ]; then
    rm -f "$PID_FILE"
    echo "❌ Web UI failed to start"
    echo "   View error log: tail -50 $LOG_FILE"
    exit 1
fi
echo "$PID" > "$PID_FILE"

for i in {1..40}; do
    if ! kill -0 "$PID" 2>/dev/null; then
        rm -f "$PID_FILE"
        echo "❌ Web UI failed to start"
        echo "   View error log: tail -50 $LOG_FILE"
        exit 1
    fi
    if NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 "http://127.0.0.1:$WEB_PORT" >/dev/null 2>&1; then
        echo "✅ Web UI started (PID: $PID, URL: http://localhost:$WEB_PORT)"
        exit 0
    fi
    sleep 1
done

echo "❌ Web UI did not become ready in time"
echo ""
echo "Troubleshooting:"
echo "  1. Check if port $WEB_PORT is in use: lsof -i :$WEB_PORT"
echo "  2. View error log: tail -50 $LOG_FILE"
echo "  3. Stop via script: make dev-web-stop"
exit 1
