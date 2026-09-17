#!/bin/bash
# Stop local astra-edge provider.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/edge-process.sh"

if [ -f "$REPO_ROOT/.env" ]; then
    set -a
    # shellcheck disable=SC1091
    source "$REPO_ROOT/.env"
    set +a
fi

PID_FILE="${ASTRA_EDGE_PID_FILE:-$REPO_ROOT/astra_edge.pid}"
LOCK_DIR="${ASTRA_EDGE_LOCK_DIR:-$REPO_ROOT/.astra_edge.lock}"
LOCK_ACQUIRED=0

if [ "${ASTRA_EDGE_LOCK_HELD:-0}" != "1" ]; then
    mkdir -p "$(dirname "$LOCK_DIR")"
    if ! mkdir "$LOCK_DIR" 2>/dev/null; then
        echo "⚠️  astra-edge start/stop is already in progress"
        exit 1
    fi
    LOCK_ACQUIRED=1
fi
trap 'if [ "$LOCK_ACQUIRED" = "1" ]; then rmdir "$LOCK_DIR" 2>/dev/null || true; fi' EXIT

kill_and_wait() {
    local pid=$1
    edge_process_is_owned "$REPO_ROOT" "$pid" || return
    kill "$pid" 2>/dev/null || return
    for _ in {1..8}; do
        sleep 1
        edge_process_is_owned "$REPO_ROOT" "$pid" || return
    done
    if edge_process_is_owned "$REPO_ROOT" "$pid"; then
        kill -9 "$pid" 2>/dev/null || true
    fi
}

if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    if edge_process_is_owned "$REPO_ROOT" "$PID"; then
        kill_and_wait "$PID"
    fi
    rm -f "$PID_FILE"
fi

echo "✅ Repo-managed astra-edge stopped"
