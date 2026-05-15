#!/bin/bash
# Start web UI dev server.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

WEB_DIR="$ROOT_DIR/web"
PID_FILE="${ASTRA_WEB_PID_FILE:-web_server.pid}"
LOG_FILE="${ASTRA_WEB_LOG_FILE:-web_server.log}"
WEB_PORT="${ASTRA_WEB_PORT:-${WEB_PORT:-3536}}"
WEB_HOST="${ASTRA_WEB_HOST:-${WEB_HOST:-127.0.0.1}}"
API_URL="${ASTRA_API_URL:-http://localhost:${ASTRA_API_PORT:-8000}}"
NEXT_BIN="$WEB_DIR/node_modules/next/dist/bin/next"

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

port_listener_pids() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -tiTCP:"$WEB_PORT" -sTCP:LISTEN 2>/dev/null || true
    else
        true
    fi
}

port_accepts_connections() {
    (echo >/dev/tcp/127.0.0.1/"$WEB_PORT") >/dev/null 2>&1
}

if [ -f "$PID_FILE" ]; then
    PID="$(cat "$PID_FILE")"
    if kill -0 "$PID" 2>/dev/null && is_web_process "$PID"; then
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

LISTENERS="$(port_listener_pids)"
if [ -n "$LISTENERS" ]; then
    echo "❌ Port $WEB_PORT is already in use by PID(s): $LISTENERS"
    echo "   Stop that process or choose a different port with ASTRA_WEB_PORT=<port>"
    exit 1
fi

if port_accepts_connections; then
    echo "❌ Port $WEB_PORT is already accepting connections"
    echo "   Stop the existing service or choose a different port with ASTRA_WEB_PORT=<port>"
    exit 1
fi

mkdir -p "$(dirname "$PID_FILE")" "$(dirname "$LOG_FILE")"
rm -f "$PID_FILE"

{
    echo ""
    echo "=== astra web dev start $(date) ==="
    echo "WEB_HOST=$WEB_HOST"
    echo "WEB_PORT=$WEB_PORT"
    echo "ASTRA_API_URL=$API_URL"
} >> "$LOG_FILE"

nohup env \
    ASTRA_API_URL="$API_URL" \
    PORT="$WEB_PORT" \
    bash -lc 'cd "$1"; shift; exec "$@"' bash "$WEB_DIR" \
    node "$NEXT_BIN" dev --hostname "$WEB_HOST" --port "$WEB_PORT" \
    >> "$LOG_FILE" 2>&1 &
PID=$!
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
