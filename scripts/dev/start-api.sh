#!/bin/bash
# Start API server

set -e

PID_FILE="api_server.pid"
LOG_FILE="api_server.log"
BIN_PATH="rust/target/release/astra-server"

echo "Starting API server..."

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

echo "Building release API binary..."
cargo build -q --manifest-path rust/Cargo.toml -p astra-runtime --release --bin astra-server
echo "✅ Using $BIN_PATH"

start_detached() {
    if command -v setsid >/dev/null 2>&1; then
        setsid "$@" >> "$LOG_FILE" 2>&1 &
    else
        nohup "$@" >> "$LOG_FILE" 2>&1 &
    fi
    DETACHED_PID=$!
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
    ASTRA_API_HOST=0.0.0.0 \
    ASTRA_API_PORT=8000 \
    "$BIN_PATH"
SETSID_PID=$DETACHED_PID
sleep 1
PID=$SETSID_PID
echo $PID > "$PID_FILE"

# Wait until the process is alive and the health endpoint reports a connected DB.
for i in {1..30}; do
    if ! kill -0 "$PID" 2>/dev/null; then
        break
    fi
    HEALTH=$(NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 http://127.0.0.1:8000/health 2>/dev/null || true)
    if echo "$HEALTH" | grep -q '"status":"healthy"' && \
       echo "$HEALTH" | grep -q '"database":"connected"'; then
        echo "✅ API server started (PID: $PID)"
        exit 0
    fi
    sleep 2
done

if kill -0 "$PID" 2>/dev/null; then
    echo "❌ API server did not become healthy in time"
else
    echo "❌ API server failed to start"
fi
echo ""
echo "Troubleshooting:"
echo "  1. Check if port 8000 is in use: lsof -i :8000"
echo "  2. View error log: tail -50 $LOG_FILE"
echo "  3. Stop via script: make dev-api-stop"
echo "  4. Check database: make dev-status"
exit 1
