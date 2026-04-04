#!/bin/bash
# Start API server

set -e

PID_FILE="api_server.pid"
LOG_FILE="api_server.log"
BIN_PATH="rust/target/debug/astra-server"

echo "Starting API server..."

# Check if already running
if [ -f "$PID_FILE" ] && kill -0 $(cat "$PID_FILE") 2>/dev/null; then
    echo "⚠️  API server already running (PID: $(cat $PID_FILE))"
    exit 0
fi

# Clean up old process record
rm -f "$PID_FILE"

# Wait for database to be ready (retry up to 30 seconds)
echo "Waiting for database..."
for i in {1..15}; do
    if bash -c 'echo >/dev/tcp/127.0.0.1/6001' 2>/dev/null; then
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

# Load .env into environment
if [ -f .env ]; then
    set -a; source .env; set +a
fi

echo "Building debug API binary..."
cargo build -q --manifest-path rust/Cargo.toml -p astra-runtime --bin astra-server
echo "✅ Using $BIN_PATH"

# Best-effort bridge autodiscovery for local dev.
# Keeps explicit CHAT_TURN_BRIDGE_URL untouched when already configured.
if [ -z "${CHAT_TURN_BRIDGE_URL:-}" ]; then
    echo "Detecting chat turn bridge endpoint..."
    for candidate in \
        "http://127.0.0.1:3001/internal/chat/turn" \
        "http://127.0.0.1:3001/api/chat/turn" \
        "http://127.0.0.1:18100/internal/chat/turn"; do
        resp_file=$(mktemp)
        header_file=$(mktemp)
        code=$(curl -s --noproxy '*' -m 2 -D "$header_file" -o "$resp_file" -w '%{http_code}' \
            -X POST "$candidate" \
            -H 'content-type: application/json' \
            -H "x-mo-bridge-secret: ${CHAT_TURN_BRIDGE_SECRET:-dev-bridge-secret-change-me}" \
            -d '{"messages":[{"role":"user","content":"ping"}]}' || true)
        ctype=$(grep -i '^content-type:' "$header_file" | head -1 | tr -d '\r' | cut -d' ' -f2- | tr '[:upper:]' '[:lower:]')
        body=$(cat "$resp_file" 2>/dev/null)
        rm -f "$header_file" "$resp_file"
        # Reject HTML responses and non-bridge JSON services (e.g. Grafana returns messageId field)
        if [[ "$ctype" == text/html* ]]; then
            continue
        fi
        if echo "$body" | grep -q '"messageId"'; then
            continue
        fi
        if [ "$code" = "200" ] || [ "$code" = "401" ] || [ "$code" = "403" ] || [ "$code" = "422" ]; then
            export CHAT_TURN_BRIDGE_URL="$candidate"
            echo "✅ CHAT_TURN_BRIDGE_URL auto-set to $CHAT_TURN_BRIDGE_URL (probe status: $code, content-type: ${ctype:-unknown})"
            break
        fi
    done
    if [ -z "${CHAT_TURN_BRIDGE_URL:-}" ]; then
        echo "⚠️  No local chat-turn bridge detected; chat will return actionable bridge-disabled error."
    fi
fi

# Start server in new session (setsid) so it's immune to Ctrl+C from parent
# Default Rust API address keeps external port 8000 behavior
setsid env http_proxy= https_proxy= HTTP_PROXY= HTTPS_PROXY= \
    RUST_API_ADDR=0.0.0.0:8000 \
    "$BIN_PATH" >> "$LOG_FILE" 2>&1 &
SETSID_PID=$!
sleep 1
PID=$SETSID_PID
echo $PID > "$PID_FILE"

# Wait and check
sleep 2
if kill -0 $PID 2>/dev/null; then
    echo "✅ API server started (PID: $PID)"
else
    echo "❌ API server failed to start"
    echo ""
    echo "Troubleshooting:"
    echo "  1. Check if port 8000 is in use: lsof -i :8000"
    echo "  2. View error log: tail -50 $LOG_FILE"
    echo "  3. Stop via script: make dev-api-stop"
    echo "  4. Check database: make dev-status"
    exit 1
fi
