#!/bin/bash
# Start API server

set -e

PID_FILE="api_server.pid"
LOG_FILE="api_server.log"

BUILD_MODE="${BUILD_MODE:-release}"
if [ "$BUILD_MODE" = "debug" ]; then
    BIN_PATH="rust/target/debug/astra-server"
else
    BIN_PATH="rust/target/release/astra-server"
fi

echo "Starting API server (mode: $BUILD_MODE)..."

# Check if already running
if [ -f "$PID_FILE" ] && kill -0 $(cat "$PID_FILE") 2>/dev/null; then
    echo "⚠️  API server already running (PID: $(cat $PID_FILE))"
    exit 0
fi

# Clean up old process record
rm -f "$PID_FILE"

# Load .env early so DB host/port are available for the readiness check
if [ -f .env ]; then
    set -a; source .env; set +a
fi

DB_HOST="${MATRIXONE_HOST:-127.0.0.1}"
DB_PORT="${MATRIXONE_PORT:-6001}"

# Wait for database to be ready (retry up to 30 seconds)
echo "Waiting for database ($DB_HOST:$DB_PORT)..."
for i in {1..15}; do
    if bash -c "echo >/dev/tcp/$DB_HOST/$DB_PORT" 2>/dev/null; then
        echo "✅ Database ready"
        break
    fi
    if [ $i -lt 15 ]; then
        echo "  Retrying... ($i/15)"
        sleep 2
    else
        echo "❌ Database not responding after 30 seconds"
        exit 1
    fi
done

# Skip build entirely with SKIP_BUILD=1 for fast iteration.
if [ "${SKIP_BUILD:-}" = "1" ] && [ -f "$BIN_PATH" ]; then
    echo "⏩ Skipping build (SKIP_BUILD=1)"
elif [ "$BUILD_MODE" = "debug" ]; then
    echo "Building debug API binary..."
    cargo build -q --manifest-path rust/Cargo.toml -p astra-runtime --bin astra-server
    echo "✅ Using $BIN_PATH"
else
    echo "Building release API binary..."
    cargo build -q --manifest-path rust/Cargo.toml -p astra-runtime --release --bin astra-server
    echo "✅ Using $BIN_PATH"
fi

start_detached() {
    if command -v setsid >/dev/null 2>&1; then
        setsid "$@" >> "$LOG_FILE" 2>&1 &
        DETACHED_PID=$!
    elif command -v screen >/dev/null 2>&1; then
        # macOS does not ship setsid(1). A plain background nohup process is
        # still tied to the launching terminal/process group in several agent
        # and IDE environments, so use screen when available to create an
        # actually detached process. The command is passed as argv rather than
        # string-concatenated so paths and env values remain shell-safe.
        screen -dmS astra-api bash -lc \
            'cd "$1"; log_file="$2"; shift 2; exec "$@" >> "$log_file" 2>&1' \
            bash "$PWD" "$LOG_FILE" "$@"
        for _ in {1..20}; do
            DETACHED_PID=$(pgrep -x "astra-server" 2>/dev/null | tail -n 1)
            if [ -n "$DETACHED_PID" ] && kill -0 "$DETACHED_PID" 2>/dev/null; then
                return
            fi
            sleep 0.2
        done
        DETACHED_PID=""
    else
        nohup "$@" >> "$LOG_FILE" 2>&1 &
        DETACHED_PID=$!
    fi
}

# Start server in a detached process so it survives Ctrl+C from parent.
# Default Rust API address keeps external port 8000 behavior.
#
# Pass proxy env vars THROUGH to the server. Only the LLM HTTP client reads
# them (via runtime/src/turn/llm_client.rs::apply_env_proxy); all other
# reqwest clients in the API server call .no_proxy() explicitly, so local /
# intranet traffic stays direct regardless of what's in the environment.
# Reaching external LLM APIs (Anthropic, OpenAI, Bedrock) from restricted
# networks requires these vars to survive, so we must NOT blank them here.
start_detached env \
    HTTPS_PROXY="${HTTPS_PROXY:-${https_proxy:-}}" \
    HTTP_PROXY="${HTTP_PROXY:-${http_proxy:-}}" \
    ALL_PROXY="${ALL_PROXY:-${all_proxy:-}}" \
    NO_PROXY="${NO_PROXY:-${no_proxy:-}}" \
    ASTRA_API_HOST="${ASTRA_API_HOST:-0.0.0.0}" \
    ASTRA_API_PORT="${ASTRA_API_PORT:-8000}" \
    "$BIN_PATH"
SETSID_PID=$DETACHED_PID
sleep 1
PID=$SETSID_PID
echo $PID > "$PID_FILE"

API_PORT="${ASTRA_API_PORT:-8000}"
API_START_TIMEOUT_SECONDS="${API_START_TIMEOUT_SECONDS:-180}"
API_HEALTH_INTERVAL_SECONDS="${API_HEALTH_INTERVAL_SECONDS:-2}"

case "$API_START_TIMEOUT_SECONDS" in
    ''|*[!0-9]*)
        echo "❌ API_START_TIMEOUT_SECONDS must be a positive integer"
        exit 1
        ;;
esac
if [ "$API_START_TIMEOUT_SECONDS" -le 0 ]; then
    echo "❌ API_START_TIMEOUT_SECONDS must be a positive integer"
    exit 1
fi
case "$API_HEALTH_INTERVAL_SECONDS" in
    ''|*[!0-9]*)
        echo "❌ API_HEALTH_INTERVAL_SECONDS must be a positive integer"
        exit 1
        ;;
esac
if [ "$API_HEALTH_INTERVAL_SECONDS" -le 0 ]; then
    echo "❌ API_HEALTH_INTERVAL_SECONDS must be a positive integer"
    exit 1
fi

# Wait until the process is alive and the health endpoint reports a connected DB.
echo "Waiting for API health (timeout: ${API_START_TIMEOUT_SECONDS}s)..."
START_SECONDS=$SECONDS
while [ $((SECONDS - START_SECONDS)) -lt "$API_START_TIMEOUT_SECONDS" ]; do
    if ! kill -0 "$PID" 2>/dev/null; then
        break
    fi
    HEALTH=$(NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 "http://127.0.0.1:${API_PORT}/health" 2>/dev/null || true)
    if echo "$HEALTH" | grep -q '"status":"healthy"' && \
       echo "$HEALTH" | grep -q '"database":"connected"'; then
        echo "✅ API server started (PID: $PID, port: $API_PORT)"
        exit 0
    fi
    sleep "$API_HEALTH_INTERVAL_SECONDS"
done

if kill -0 "$PID" 2>/dev/null; then
    echo "❌ API server did not become healthy in time"
    echo "Stopping unhealthy API server (PID: $PID)..."
    kill "$PID" 2>/dev/null || true
    for _ in {1..20}; do
        if ! kill -0 "$PID" 2>/dev/null; then
            break
        fi
        sleep 0.2
    done
    if kill -0 "$PID" 2>/dev/null; then
        kill -9 "$PID" 2>/dev/null || true
    fi
else
    echo "❌ API server failed to start"
fi
rm -f "$PID_FILE"
echo ""
echo "Troubleshooting:"
echo "  1. Check if port $API_PORT is in use: lsof -i :$API_PORT"
echo "  2. View error log: tail -50 $LOG_FILE"
echo "  3. Stop via script: make dev-api-stop"
echo "  4. Check database: make dev-status"
exit 1
