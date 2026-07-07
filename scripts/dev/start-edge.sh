#!/bin/bash
# Start local astra-edge provider for web/server runtime sessions.

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
LOG_FILE="${ASTRA_EDGE_LOG_FILE:-$REPO_ROOT/astra_edge.log}"
LOCK_DIR="${ASTRA_EDGE_LOCK_DIR:-$REPO_ROOT/.astra_edge.lock}"
BUILD_MODE="${BUILD_MODE:-debug}"
WORKSPACE_DIR="${ASTRA_EDGE_WORKSPACE_DIR:-$PWD}"

mkdir -p "$(dirname "$PID_FILE")" "$(dirname "$LOG_FILE")" "$(dirname "$LOCK_DIR")"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    echo "⚠️  astra-edge start/stop is already in progress"
    exit 1
fi
trap 'rmdir "$LOCK_DIR" 2>/dev/null || true' EXIT

if [ "$BUILD_MODE" = "release" ]; then
    BIN_PATH="$REPO_ROOT/target/release/astra-edge"
else
    BIN_PATH="$REPO_ROOT/target/debug/astra-edge"
fi

echo "Starting astra-edge provider (mode: $BUILD_MODE, workspace: $WORKSPACE_DIR)..."

RUNNING_PIDS=$(pgrep -x "astra-edge" 2>/dev/null || true)
if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    PID=$(cat "$PID_FILE")
    EXTRA_PIDS=$(echo "$RUNNING_PIDS" | tr ' ' '\n' | grep -v "^${PID}$" || true)
    if [ -z "$EXTRA_PIDS" ]; then
        echo "⚠️  astra-edge already running (PID: $PID)"
        exit 0
    fi
    echo "⚠️  Multiple astra-edge processes detected; restarting the provider"
    ASTRA_EDGE_LOCK_HELD=1 "$SCRIPT_DIR/stop-edge.sh" >/dev/null 2>&1 || true
elif [ -n "$RUNNING_PIDS" ]; then
    echo "⚠️  Stale astra-edge process detected; restarting the provider"
    ASTRA_EDGE_LOCK_HELD=1 "$SCRIPT_DIR/stop-edge.sh" >/dev/null 2>&1 || true
fi

if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    echo "⚠️  astra-edge already running (PID: $(cat "$PID_FILE"))"
    exit 0
fi
rm -f "$PID_FILE"

API_PORT="${ASTRA_API_PORT:-17001}"
SERVER_URL="${ASTRA_EDGE_SERVER_URL:-${ASTRA_SERVER_URL:-${ASTRA_API_URL:-http://127.0.0.1:${API_PORT}}}}"

edge_health_url() {
    local base="${1%/}"
    base="${base%%\?*}"
    base="${base%%#*}"
    case "$base" in
        ws://*) base="http://${base#ws://}" ;;
        wss://*) base="https://${base#wss://}" ;;
    esac
    case "$base" in
        */edge/ws*) base="${base%%/edge/ws*}/edge/ws" ;;
    esac
    base="${base%/edge/ws}"
    echo "$base/health"
}

SERVER_HEALTH_URL="$(edge_health_url "$SERVER_URL")"

if ! NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 "$SERVER_HEALTH_URL" >/dev/null 2>&1; then
    echo "❌ Astra API is not healthy at $SERVER_HEALTH_URL"
    echo "   Start server-only first: make dev-start-server-only"
    exit 1
fi

if [ "$BUILD_MODE" = "release" ]; then
    echo "Building release astra-edge binary..."
    cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p astra-edge --release --bin astra-edge
else
    echo "Building debug astra-edge binary..."
    cargo build -q --manifest-path "$REPO_ROOT/Cargo.toml" -p astra-edge --bin astra-edge
fi

if ! NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 "$SERVER_HEALTH_URL" >/dev/null 2>&1; then
    echo "❌ Astra API stopped responding at $SERVER_HEALTH_URL after astra-edge build"
    echo "   Restart server-only first: make dev-start-server-only"
    exit 1
fi

{
    echo ""
    echo "=== astra-edge start $(date) ==="
    echo "ASTRA_EDGE_SERVER_URL=$SERVER_URL"
    echo "ASTRA_EDGE_WORKSPACE_DIR=$WORKSPACE_DIR"
} >> "$LOG_FILE"

ARGS=(
    --server-url "$SERVER_URL"
    --workspace-dir "$WORKSPACE_DIR"
)
if [ -n "${ASTRA_EDGE_ID:-}" ]; then
    ARGS+=(--edge-id "$ASTRA_EDGE_ID")
fi
if [ -n "${ASTRA_EDGE_PROFILE:-}" ]; then
    ARGS+=(--profile "$ASTRA_EDGE_PROFILE")
fi

if command -v setsid >/dev/null 2>&1; then
    setsid "$BIN_PATH" "${ARGS[@]}" >> "$LOG_FILE" 2>&1 &
    PID=$!
else
    nohup "$BIN_PATH" "${ARGS[@]}" >> "$LOG_FILE" 2>&1 &
    PID=$!
fi

if [ -z "${PID:-}" ] || ! kill -0 "$PID" 2>/dev/null; then
    echo "❌ astra-edge failed to start"
    echo "   View error log: tail -50 $LOG_FILE"
    exit 1
fi
echo "$PID" > "$PID_FILE"

for _ in {1..30}; do
    if ! kill -0 "$PID" 2>/dev/null; then
        rm -f "$PID_FILE"
        echo "❌ astra-edge exited during startup"
        echo "   View error log: tail -50 $LOG_FILE"
        exit 1
    fi
    if grep -q "Edge agent ready" "$LOG_FILE" 2>/dev/null; then
        echo "✅ astra-edge started (PID: $PID, workspace: $WORKSPACE_DIR)"
        exit 0
    fi
    if grep -q "Authentication failed\\|profile .* is not logged in\\|no profile" "$LOG_FILE" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        rm -f "$PID_FILE"
        echo "❌ astra-edge authentication failed"
        echo "   Run astra login, or set ASTRA_TOKEN, then retry."
        echo "   View error log: tail -50 $LOG_FILE"
        exit 1
    fi
    sleep 1
done

echo "⚠️  astra-edge process is running, but readiness was not confirmed yet"
echo "   PID: $PID"
echo "   Check: make dev-edge-logs"
