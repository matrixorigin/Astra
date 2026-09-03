#!/usr/bin/env bash
# Verify the running all-in-one stack through its public HTTP boundaries.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
env_file="${1:-$repo_root/deployment/all-in-one/.env}"

if [[ ! -f "$env_file" ]]; then
    echo "all-in-one verification failed: missing environment file: $env_file" >&2
    exit 1
fi
for command in curl python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "all-in-one verification failed: $command is required" >&2
        exit 1
    fi
done

# shellcheck source=../lib/env_file.sh
. "$repo_root/scripts/lib/env_file.sh"

api_port="$(env_resolve_value "$env_file" ASTRA_API_PORT 2>/dev/null || true)"
memoria_port="$(env_resolve_value "$env_file" MEMORIA_PORT 2>/dev/null || true)"
memoria_key="$(env_resolve_value "$env_file" MEMORIA_MASTER_KEY 2>/dev/null || true)"
bind_address="$(env_resolve_value "$env_file" ASTRA_BIND_ADDRESS 2>/dev/null || true)"
http_host="$(env_http_host_from_bind "$bind_address")"
api_url="${ASTRA_SMOKE_API_URL:-http://${http_host}:${api_port:-17001}}"
memoria_url="${ASTRA_SMOKE_MEMORIA_URL:-http://${http_host}:${memoria_port:-8100}}"

if [[ -z "$memoria_key" ]]; then
    echo "all-in-one verification failed: MEMORIA_MASTER_KEY is empty" >&2
    exit 1
fi

http_request() {
    local stage="$1"
    local url="$2"
    local response
    shift 2
    if ! response="$(curl --noproxy '*' --fail-with-body --silent --show-error "$@" "$url")"; then
        echo "all-in-one verification failed during ${stage}: ${url}" >&2
        if [[ -n "$response" ]]; then
            echo "Service response: $response" >&2
        fi
        return 1
    fi
    printf '%s' "$response"
}

# Feed the master key to curl over stdin so it never appears in the curl
# process arguments on a shared development host.
memoria_request() {
    local stage="$1"
    local url="$2"
    shift 2
    printf 'Authorization: Bearer %s\n' "$memoria_key" |
        http_request "$stage" "$url" --header @- "$@"
}

ready_response="$(http_request "Astra readiness" "$api_url/ready" --max-time 15)"
printf '%s' "$ready_response" | python3 -c '
import json
import sys

try:
    response = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"Astra readiness returned invalid JSON: {error}")
if not isinstance(response, dict):
    raise SystemExit(f"Astra readiness returned an unexpected payload: {response}")
if response.get("status") != "ready" or response.get("database") != "connected":
    raise SystemExit(f"Astra is not ready: {response}")
'
echo "✅ Astra readiness: ready, database connected"

health_response="$(http_request "Astra dependency health" "$api_url/health" --max-time 15)"
printf '%s' "$health_response" | python3 -c '
import json
import sys

try:
    response = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"Astra health returned invalid JSON: {error}")
if not isinstance(response, dict):
    raise SystemExit(f"Astra health returned an unexpected payload: {response}")
expected = {"status": "healthy", "database": "connected", "memoria": "connected"}
if any(response.get(key) != value for key, value in expected.items()):
    raise SystemExit(f"Astra dependencies are not healthy: {response}")
'
echo "✅ Astra health: database and Memoria connected"

http_request "Memoria health" "$memoria_url/health" --max-time 15 >/dev/null
echo "✅ Memoria health: HTTP 2xx"

test_user="astra-all-in-one-smoke"
test_session="astra-all-in-one-smoke-session"
nonce="$(python3 -c 'import secrets, string; print("".join(secrets.choice(string.ascii_lowercase) for _ in range(16)))')"
content="Astra violet cedar ${nonce} embedding round trip"
memory_id=""
cleanup_memory() {
    if [[ -z "$memory_id" ]]; then
        return
    fi
    if ! memoria_request "smoke memory cleanup" "$memoria_url/v1/memories/purge" --max-time 15 \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"memory_ids\":[\"${memory_id}\"],\"reason\":\"all-in-one verification cleanup\"}" \
        >/dev/null; then
        echo "⚠️  Could not remove smoke memory ${memory_id}; remove it manually." >&2
    fi
}
trap cleanup_memory EXIT

store_response="$(
    memoria_request "memory store" "$memoria_url/v1/memories" --max-time 30 \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"content\":\"${content}\",\"memory_type\":\"semantic\",\"session_id\":\"${test_session}\"}"
)"
memory_id="$(printf '%s' "$store_response" | python3 -c '
import json
import sys

try:
    response = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"memory store returned invalid JSON: {error}")
if not isinstance(response, dict):
    raise SystemExit(f"memory store returned an unexpected payload: {response}")
memory_id = response.get("memory_id")
if not isinstance(memory_id, str) or not memory_id:
    raise SystemExit(f"store response has no memory_id: {response}")
print(memory_id)
')"
echo "✅ Memory store: returned an ID"

retrieve_response="$(
    memoria_request "memory retrieval" "$memoria_url/v1/memories/retrieve" --max-time 30 \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"query\":\"violet cedar ${nonce} embedding\",\"top_k\":10,\"session_id\":\"${test_session}\",\"session_scope\":\"only\"}"
)"
printf '%s' "$retrieve_response" | CONTENT="$content" MEMORY_ID="$memory_id" python3 -c '
import json
import os
import sys

try:
    response = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"memory retrieval returned invalid JSON: {error}")
rows = response.get("memories", response) if isinstance(response, dict) else response
if not isinstance(rows, list):
    raise SystemExit(f"retrieve response is not a memory list: {response}")
if not any(
    row.get("memory_id") == os.environ["MEMORY_ID"]
    and row.get("content") == os.environ["CONTENT"]
    for row in rows
):
    raise SystemExit("retrieve did not return the exact memory just stored")
'
echo "✅ Memory retrieval: exact stored ID and content returned"

purge_response="$(
    memoria_request "memory cleanup" "$memoria_url/v1/memories/purge" --max-time 15 \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"memory_ids\":[\"${memory_id}\"],\"reason\":\"all-in-one verification cleanup\"}"
)"
printf '%s' "$purge_response" | python3 -c '
import json
import sys

try:
    response = json.load(sys.stdin)
except (json.JSONDecodeError, UnicodeDecodeError) as error:
    raise SystemExit(f"memory cleanup returned invalid JSON: {error}")
if not isinstance(response, dict):
    raise SystemExit(f"memory cleanup returned an unexpected payload: {response}")
purged = response.get("purged", response.get("deleted_count", 0))
if not isinstance(purged, int) or purged < 1:
    raise SystemExit(f"smoke memory was not removed: {response}")
'
memory_id=""
echo "✅ Memory cleanup: removed smoke record"
echo "✅ All-in-one runtime verification passed"
