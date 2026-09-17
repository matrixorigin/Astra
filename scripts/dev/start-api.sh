#!/bin/bash
# Start API server

set -e

PID_FILE="api_server.pid"
LOG_FILE="api_server.log"
REPO_ROOT="$(pwd -P)"
# shellcheck source=../lib/api_identity.sh
. "$REPO_ROOT/scripts/lib/api_identity.sh"

BUILD_MODE="${BUILD_MODE:-release}"
if [ "$BUILD_MODE" = "debug" ]; then
    BIN_PATH="target/debug/astra-server"
else
    BIN_PATH="target/release/astra-server"
fi

echo "Starting API server (mode: $BUILD_MODE)..."

# High-concurrency local probes keep one inbound SSE socket per client and open
# outbound DB/LLM/Memoria sockets from the same process. macOS shells commonly
# start with a 256 soft nofile limit, which is too low for 200+ client probes.
CURRENT_NOFILE=$(ulimit -n)
TARGET_NOFILE=4096
case "$CURRENT_NOFILE" in
    ''|*[!0-9]*)
        echo "⚠️  Could not inspect file descriptor limit: $CURRENT_NOFILE"
        ;;
    *)
        if [ "$CURRENT_NOFILE" -lt "$TARGET_NOFILE" ]; then
            if ulimit -n "$TARGET_NOFILE" 2>/dev/null; then
                echo "✅ File descriptor limit raised to $(ulimit -n)"
            else
                echo "⚠️  File descriptor limit remains $CURRENT_NOFILE; high-concurrency probes may fail"
            fi
        else
            echo "✅ File descriptor limit: $CURRENT_NOFILE"
        fi
        ;;
esac

# Load the selected env file early so DB host/port are available for the
# readiness check. ASTRA_ENV_FILE lets cross-repository local harnesses use an
# isolated configuration without rewriting a developer's normal .env.
ENV_FILE="${ASTRA_ENV_FILE:-.env}"
if [ -f "$ENV_FILE" ]; then
    set -a; source "$ENV_FILE"; set +a
fi

# A caller-selected env file is an explicit configuration boundary. Prevent
# the server's own config loader from filling missing values from the repo
# .env or user/system config after this script has deliberately omitted them.
if [ -n "${ASTRA_ENV_FILE:-}" ]; then
    export ASTRA_CONFIG_SOURCE="${ASTRA_CONFIG_SOURCE:-explicit-env}"
fi

# Cloud BYOK must resolve a per-user Memoria credential. This explicit switch
# guarantees an inherited shell variable cannot silently re-enable master-key
# fallback in the local cross-repository test topology.
if [ "${ASTRA_DISABLE_MEMORIA_MASTER_KEY:-}" = "1" ]; then
    unset MEMORIA_MASTER_KEY
fi

# Service reachability does not imply that local accounts can use memory.
if [[ -n "${MEMORIA_MASTER_KEY:-}" && -z "${MEMORIA_WEB_URL:-}" && "${MEMORIA_SELF_HOSTED_MASTER_ACCESS:-0}" != 1 ]]; then
    echo "⚠️  Memoria is configured but local user memory is disabled. Set MEMORIA_SELF_HOSTED_MASTER_ACCESS=1 in $ENV_FILE for self-hosted Memoria 0.5.2+."
fi

API_PORT="${ASTRA_API_PORT:-17001}"
DB_HOST="${MATRIXONE_HOST:-127.0.0.1}"
DB_PORT="${MATRIXONE_PORT:-6001}"
HEALTH_URL="http://127.0.0.1:${API_PORT}/health"
READY_URL="http://127.0.0.1:${API_PORT}/ready"

# A ready process is reusable only when its process identity, source revision,
# and checkout state are provable. Reusing a healthy process from another
# worktree makes the Web UI send a newer Work contract to an older Server,
# which then reports a misleading invalid JSON request. The health response
# carries the build identity; the process cwd closes the same-commit,
# different-worktree gap. A dirty checkout is never silently reused because
# the health contract does not expose the uncommitted source identity.
api_build_git_sha() {
    NO_PROXY=localhost,127.0.0.1 curl -s \
        --connect-timeout 1 --max-time 2 "$HEALTH_URL" 2>/dev/null |
        api_health_build_git_sha
}

api_build_git_dirty() {
    NO_PROXY=localhost,127.0.0.1 curl -s \
        --connect-timeout 1 --max-time 2 "$HEALTH_URL" 2>/dev/null |
        api_health_build_git_dirty
}

current_git_sha() {
    git -C "$REPO_ROOT" rev-parse --verify HEAD 2>/dev/null || true
}

checkout_is_clean() {
    [ -z "$(git -C "$REPO_ROOT" status --porcelain=v1 --untracked-files=no 2>/dev/null)" ]
}

checkout_git_dirty() {
    if checkout_is_clean; then
        printf 'false\n'
    else
        printf 'true\n'
    fi
}

process_cwd() {
    local pid=$1
    local cwd
    cwd=$(readlink -f "/proc/$pid/cwd" 2>/dev/null || true)
    if [ -z "$cwd" ] && command -v lsof >/dev/null 2>&1; then
        cwd=$(lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
    fi
    printf '%s\n' "$cwd"
}

process_is_this_checkout() {
    local pid=$1
    local cwd
    [ -n "$pid" ] || return 1
    cwd=$(process_cwd "$pid")
    [ -n "$cwd" ] && [ "$cwd" = "$REPO_ROOT" ]
}

api_build_matches_head() {
    local expected actual
    expected=$(current_git_sha)
    actual=$(api_build_git_sha)
    [ -n "$expected" ] && [ -n "$actual" ] && [ "$expected" = "$actual" ]
}

api_reusable() {
    local pid=$1
    process_is_this_checkout "$pid" &&
        api_build_matches_head &&
        [ "$(api_build_git_dirty)" = "false" ] &&
        checkout_is_clean
}

api_started_from_this_checkout() {
    local pid=$1
    kill -0 "$pid" 2>/dev/null &&
        api_build_matches_head &&
        [ "$(api_build_git_dirty)" = "$(checkout_git_dirty)" ]
}

api_identity_mismatch() {
    api_health_identity_mismatch_from_url \
        "$(current_git_sha)" "$(checkout_git_dirty)" "$HEALTH_URL"
}

prepare_build_identity() {
    export ASTRA_BUILD_SOURCE_GIT_SHA="$(current_git_sha)"
    export ASTRA_BUILD_SOURCE_GIT_DIRTY="$(checkout_git_dirty)"
}

existing_api_pid() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -tiTCP:"$API_PORT" -sTCP:LISTEN 2>/dev/null | head -n 1
    fi
}

# Startup readiness is owned by the core API dependency: the primary
# database.  Optional capabilities (currently Memoria) are reported by
# /health as degraded and must not prevent a usable API from starting.
api_ready() {
    local status
    status=$(NO_PROXY=localhost,127.0.0.1 curl -s \
        --connect-timeout 1 --max-time 2 \
        -o /dev/null -w '%{http_code}' "$READY_URL" 2>/dev/null || true)
    [ "$status" = "200" ]
}

# A PID file is only an optimization. Never let it turn a process from an
# earlier checkout or commit into the API for this one.
if [ -f "$PID_FILE" ]; then
    PID_FROM_FILE=$(sed -n '1p' "$PID_FILE" 2>/dev/null || true)
    if [ -n "$PID_FROM_FILE" ] && kill -0 "$PID_FROM_FILE" 2>/dev/null; then
        if api_ready && api_reusable "$PID_FROM_FILE"; then
            echo "⚠️  API server already ready for this checkout (PID: $PID_FROM_FILE)"
            exit 0
        fi
        if process_is_this_checkout "$PID_FROM_FILE"; then
            echo "❌ API server PID $PID_FROM_FILE belongs to this checkout but cannot be safely reused."
            echo "   Stop it with 'make dev-api-stop', then start the API again."
            exit 1
        fi
        echo "⚠️  Ignoring stale API PID file: $PID_FILE"
    fi
    rm -f "$PID_FILE"
fi

# Recover from an earlier launcher losing its PID after the server became
# ready (notably the macOS screen branch). Starting a second server would
# only produce a misleading bind failure while the first instance is usable.
if api_ready; then
    EXISTING_PID="$(existing_api_pid)"
    if [ -z "$EXISTING_PID" ] || ! api_reusable "$EXISTING_PID"; then
        echo "❌ API port $API_PORT is served by a Server that cannot be proven to belong to this checkout."
        echo "   Current checkout: $(current_git_sha)"
        echo "   Running build:    $(api_build_git_sha || echo unknown)"
        echo "   Running source:   $(api_build_git_dirty || echo unknown)"
        echo "   Stop that Server, then restart it from this checkout before opening Web."
        exit 1
    fi
    if [ -n "$EXISTING_PID" ] && kill -0 "$EXISTING_PID" 2>/dev/null; then
        echo "$EXISTING_PID" > "$PID_FILE"
        echo "⚠️  API server already ready (PID: $EXISTING_PID, port: $API_PORT)"
    else
        echo "⚠️  API server already ready (port: $API_PORT; PID unavailable)"
    fi
    exit 0
fi

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
prepare_build_identity
if [ "${SKIP_BUILD:-}" = "1" ] && [ -f "$BIN_PATH" ]; then
    if ! checkout_is_clean; then
        echo "❌ SKIP_BUILD=1 cannot safely start a binary from a dirty checkout."
        echo "   Rebuild without SKIP_BUILD so the Server records the current source state."
        exit 1
    fi
    echo "⏩ Skipping build (SKIP_BUILD=1)"
elif [ "$BUILD_MODE" = "debug" ]; then
    echo "Building debug API binary..."
    cargo build -q --manifest-path Cargo.toml -p astra-runtime --bin astra-server
    echo "✅ Using $BIN_PATH"
else
    echo "Building release API binary..."
    cargo build -q --manifest-path Cargo.toml -p astra-runtime --release --bin astra-server
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
        SCREEN_SESSION="astra-api-$$"
        ABS_PID_FILE="$PWD/$PID_FILE"
        screen -dmS "$SCREEN_SESSION" bash -lc \
            'cd "$1"; log_file="$2"; pid_file="$3"; shift 3; printf "%s\n" "$$" > "$pid_file"; exec "$@" >> "$log_file" 2>&1' \
            bash "$PWD" "$LOG_FILE" "$ABS_PID_FILE" "$@"
        for _ in {1..100}; do
            DETACHED_PID=$(sed -n '1p' "$ABS_PID_FILE" 2>/dev/null || true)
            if [ -n "$DETACHED_PID" ] && kill -0 "$DETACHED_PID" 2>/dev/null; then
                return
            fi
            sleep 0.2
        done
        screen -S "$SCREEN_SESSION" -X quit 2>/dev/null || true
        DETACHED_PID=""
    else
        nohup "$@" >> "$LOG_FILE" 2>&1 &
        DETACHED_PID=$!
    fi
}

# Start server in a detached process so it survives Ctrl+C from parent.
# Default Rust API address keeps external port 17001 behavior.
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
    ASTRA_API_PORT="${ASTRA_API_PORT:-17001}" \
    "$BIN_PATH"
SETSID_PID=$DETACHED_PID
sleep 1
PID=$SETSID_PID
echo $PID > "$PID_FILE"

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

# Wait until the process is alive and the core readiness endpoint accepts
# traffic.  /health may legitimately report `degraded` while an optional
# capability is unavailable.
echo "Waiting for API readiness (timeout: ${API_START_TIMEOUT_SECONDS}s)..."
START_SECONDS=$SECONDS
STARTUP_IDENTITY_MISMATCH=0
while [ $((SECONDS - START_SECONDS)) -lt "$API_START_TIMEOUT_SECONDS" ]; do
    if ! kill -0 "$PID" 2>/dev/null; then
        break
    fi
    if api_ready; then
        if api_started_from_this_checkout "$PID"; then
            echo "✅ API server started (PID: $PID, port: $API_PORT)"
            exit 0
        fi
        if api_identity_mismatch; then
            STARTUP_IDENTITY_MISMATCH=1
            break
        fi
    fi
    sleep "$API_HEALTH_INTERVAL_SECONDS"
done

if kill -0 "$PID" 2>/dev/null; then
    if [ "$STARTUP_IDENTITY_MISMATCH" -eq 1 ]; then
        echo "❌ API server started, but its build identity does not match this checkout"
        echo "   Current checkout: $(current_git_sha)"
        echo "   Running build:    $(api_build_git_sha || echo unknown)"
        echo "   Running source:   $(api_build_git_dirty || echo unknown)"
    else
        echo "❌ API server did not become ready in time"
    fi
    echo "Last /ready response:"
    NO_PROXY=localhost,127.0.0.1 curl -sS --connect-timeout 1 --max-time 2 \
        "$READY_URL" 2>/dev/null || true
    echo "Last /health response:"
    NO_PROXY=localhost,127.0.0.1 curl -sS --connect-timeout 1 --max-time 2 \
        "$HEALTH_URL" 2>/dev/null || true
    echo "Recent API log:"
    tail -20 "$LOG_FILE" 2>/dev/null || true
    echo "Stopping API server (PID: $PID)..."
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
