#!/bin/bash
# Run the built web agent server in the foreground.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/dev/web-config.sh
source "$ROOT_DIR/scripts/dev/web-config.sh"

WEB_DIR="$ROOT_DIR/web"
WEB_PORT="$(web_agent_port)"
WEB_HOST="$(web_agent_host)"
NEXT_BIN="$WEB_DIR/node_modules/next/dist/bin/next"

web_validate_port "$WEB_PORT"

if ! command -v node >/dev/null 2>&1; then
    echo "node is required to start the web UI"
    exit 1
fi

if [ ! -f "$NEXT_BIN" ]; then
    echo "Web dependencies are not installed"
    echo "Run: make dev-web-deps"
    exit 1
fi

LISTENERS="$(web_port_listener_pids "$WEB_PORT")"
if [ -n "$LISTENERS" ]; then
    echo "Port $WEB_PORT is already in use by PID(s): $LISTENERS"
    echo "Stop the existing web agent process before starting a new one."
    exit 1
fi

if web_port_accepts_connections "$WEB_PORT"; then
    echo "Port $WEB_PORT is already accepting connections"
    echo "Stop the existing web agent process before starting a new one."
    exit 1
fi

cd "$WEB_DIR"
export PORT="$WEB_PORT"
exec node "$NEXT_BIN" start --hostname "$WEB_HOST" --port "$WEB_PORT"
