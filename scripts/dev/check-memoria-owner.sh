#!/usr/bin/env bash
# Verify the Memoria contract Astra actually uses for self-hosted memory.
#
# `/health` and `/v1/health/analyze` only prove that the process and its
# administrator bearer credential are alive.  Astra's local-user path uses
# the non-admin Memoria-Owner scheme, so a backend can look healthy while
# every session-memory operation returns 401.  This probe is read-only: it
# retrieves a deliberately unique, nonexistent query and does not execute an
# explicit memory write.

set -euo pipefail

base_url="${MEMORIA_BASE_URL:-http://127.0.0.1:${MEMORIA_PORT:-8100}}"
base_url="${base_url%/}"

# Hosted/browser-login deployments use a scoped user credential and must not
# be forced through the local master-key owner probe.  The self-hosted flag is
# explicit by design; an unset value remains an opt-out.
if [[ "${MEMORIA_SELF_HOSTED_MASTER_ACCESS:-0}" != "1" || -n "${MEMORIA_WEB_URL:-}" ]]; then
    echo "ℹ️  Memoria owner-auth probe skipped (self-hosted master access is disabled)"
    exit 0
fi

master_key="${MEMORIA_MASTER_KEY:-}"
if [[ -z "$master_key" ]]; then
    echo "❌ MEMORIA_MASTER_KEY is required for the self-hosted Memoria-Owner readiness check" >&2
    exit 2
fi

retries="${MEMORIA_OWNER_READY_RETRIES:-60}"
delay="${MEMORIA_OWNER_READY_RETRY_DELAY_SECONDS:-2}"
case "$retries" in
    ''|0|0*|*[!0-9]*)
        echo "❌ MEMORIA_OWNER_READY_RETRIES must be a positive integer: $retries" >&2
        exit 2
        ;;
esac
case "$delay" in
    ''|*[!0-9]*)
        echo "❌ MEMORIA_OWNER_READY_RETRY_DELAY_SECONDS must be a non-negative integer: $delay" >&2
        exit 2
        ;;
esac

probe_user="astra-owner-readiness-probe"
probe_body='{"query":"__astra_owner_auth_readiness_probe__","top_k":1}'
response_limit=65536
response_file="$(mktemp)"
trap 'rm -f "$response_file"' EXIT

for attempt in $(seq 1 "$retries"); do
    : >"$response_file"
    curl_exit=0
    status="$(curl --noproxy '*' --silent --show-error \
        --connect-timeout 1 --max-time 5 \
        --max-filesize "$response_limit" \
        -o "$response_file" -w '%{http_code}' \
        -H "Authorization: Memoria-Owner ${master_key}" \
        -H "X-User-Id: ${probe_user}" \
        -H 'Content-Type: application/json' \
        -X POST "${base_url}/v1/memories/retrieve" \
        --data "$probe_body" 2>/dev/null)" || curl_exit=$?

    if [[ "$curl_exit" -ne 0 ]]; then
        detail="transport failed (curl exit ${curl_exit}; HTTP ${status:-000})"
    else
        case "$status" in
            2??)
                response_size="$(wc -c <"$response_file")"
                if [[ "$response_size" -le "$response_limit" ]] && python3 -c '
import json, sys
value = json.load(sys.stdin)
valid = isinstance(value, list) or (
    isinstance(value, dict)
    and "error" not in value
    and (isinstance(value.get("memories"), list) or isinstance(value.get("results"), list))
)
raise SystemExit(0 if valid else 1)
' <"$response_file" >/dev/null 2>&1; then
                    echo "✅ Memoria owner-authenticated storage is ready"
                    exit 0
                fi
                if [[ "$response_size" -gt "$response_limit" ]]; then
                    detail="response exceeded 64 KiB"
                else
                    detail="response was not valid JSON"
                fi
                ;;
            401)
                echo "❌ Memoria owner authentication failed (HTTP 401)." >&2
                echo "   The backend must support Memoria-Owner (Memoria 0.5.2+), and MEMORIA_MASTER_KEY must match." >&2
                exit 1
                ;;
            403)
                echo "❌ Memoria rejected owner authentication (HTTP 403)." >&2
                echo "   Check MEMORIA_SELF_HOSTED_MASTER_ACCESS=1 and the local-user deployment policy." >&2
                exit 1
                ;;
            *)
                detail="HTTP ${status:-000}"
                ;;
        esac
    fi

    if [[ "$attempt" -eq "$retries" ]]; then
        echo "❌ Memoria owner-authenticated storage was not ready after ${retries} attempt(s): $detail" >&2
        echo "   Check 'make dev-deps-logs-once' and confirm the Memoria databases exist." >&2
        exit 1
    fi
    echo "  Waiting for Memoria owner-authenticated storage... ($attempt/$retries)"
    sleep "$delay"
done

echo "❌ Memoria owner-auth readiness loop ended unexpectedly" >&2
exit 1
