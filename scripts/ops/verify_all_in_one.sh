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
api_url="${ASTRA_SMOKE_API_URL:-http://127.0.0.1:${api_port:-17001}}"
memoria_url="${ASTRA_SMOKE_MEMORIA_URL:-http://127.0.0.1:${memoria_port:-8100}}"

if [[ -z "$memoria_key" ]]; then
    echo "all-in-one verification failed: MEMORIA_MASTER_KEY is empty" >&2
    exit 1
fi

ready_response="$(curl --noproxy '*' --fail-with-body --silent --show-error --max-time 15 "$api_url/ready")"
printf '%s' "$ready_response" | python3 -c '
import json
import sys

response = json.load(sys.stdin)
if response.get("status") != "ready" or response.get("database") != "connected":
    raise SystemExit(f"Astra is not ready: {response}")
'
echo "✅ Astra readiness: ready, database connected"

health_response="$(curl --noproxy '*' --fail-with-body --silent --show-error --max-time 15 "$api_url/health")"
printf '%s' "$health_response" | python3 -c '
import json
import sys

response = json.load(sys.stdin)
expected = {"status": "healthy", "database": "connected", "memoria": "connected"}
if any(response.get(key) != value for key, value in expected.items()):
    raise SystemExit(f"Astra dependencies are not healthy: {response}")
'
echo "✅ Astra health: database and Memoria connected"

curl --noproxy '*' --fail-with-body --silent --show-error --max-time 15 "$memoria_url/health" >/dev/null
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
    if ! curl --noproxy '*' --fail-with-body --silent --show-error --max-time 15 \
        -H "Authorization: Bearer ${memoria_key}" \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"memory_ids\":[\"${memory_id}\"],\"reason\":\"all-in-one verification cleanup\"}" \
        "$memoria_url/v1/memories/purge" >/dev/null; then
        echo "⚠️  Could not remove smoke memory ${memory_id}; remove it manually." >&2
    fi
}
trap cleanup_memory EXIT

store_response="$(
    curl --noproxy '*' --fail-with-body --silent --show-error --max-time 30 \
        -H "Authorization: Bearer ${memoria_key}" \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"content\":\"${content}\",\"memory_type\":\"semantic\",\"session_id\":\"${test_session}\"}" \
        "$memoria_url/v1/memories"
)"
memory_id="$(printf '%s' "$store_response" | python3 -c '
import json
import sys

response = json.load(sys.stdin)
memory_id = response.get("memory_id")
if not isinstance(memory_id, str) or not memory_id:
    raise SystemExit(f"store response has no memory_id: {response}")
print(memory_id)
')"
echo "✅ Memory store: returned an ID"

retrieve_response="$(
    curl --noproxy '*' --fail-with-body --silent --show-error --max-time 30 \
        -H "Authorization: Bearer ${memoria_key}" \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"query\":\"violet cedar ${nonce} embedding\",\"top_k\":10,\"session_id\":\"${test_session}\",\"session_scope\":\"only\"}" \
        "$memoria_url/v1/memories/retrieve"
)"
printf '%s' "$retrieve_response" | CONTENT="$content" MEMORY_ID="$memory_id" python3 -c '
import json
import os
import sys

response = json.load(sys.stdin)
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
    curl --noproxy '*' --fail-with-body --silent --show-error --max-time 15 \
        -H "Authorization: Bearer ${memoria_key}" \
        -H "X-User-Id: ${test_user}" \
        -H 'Content-Type: application/json' \
        --data "{\"memory_ids\":[\"${memory_id}\"],\"reason\":\"all-in-one verification cleanup\"}" \
        "$memoria_url/v1/memories/purge"
)"
printf '%s' "$purge_response" | python3 -c '
import json
import sys

response = json.load(sys.stdin)
purged = response.get("purged", response.get("deleted_count", 0))
if not isinstance(purged, int) or purged < 1:
    raise SystemExit(f"smoke memory was not removed: {response}")
'
memory_id=""
echo "✅ Memory cleanup: removed smoke record"
echo "✅ All-in-one runtime verification passed"
