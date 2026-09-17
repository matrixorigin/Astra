#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-memoria-owner.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

stub_bin="$test_root/bin"
mkdir -p "$stub_bin"
cat >"$stub_bin/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

response_file=""
while (($#)); do
    case "$1" in
        -o)
            response_file="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done
printf '[]' >"$response_file"
printf '200'
# Simulate a connection that returned an HTTP response but failed before the
# transfer was complete. The readiness probe must not accept the valid prefix.
exit 18
STUB
chmod +x "$stub_bin/curl"

if PATH="$stub_bin:$PATH" \
    MEMORIA_SELF_HOSTED_MASTER_ACCESS=1 \
    MEMORIA_WEB_URL= \
    MEMORIA_MASTER_KEY=contract-key \
    MEMORIA_OWNER_READY_RETRIES=1 \
    MEMORIA_OWNER_READY_RETRY_DELAY_SECONDS=0 \
    "$repo_root/scripts/dev/check-memoria-owner.sh" >"$test_root/output.log" 2>&1; then
    echo "owner probe accepted a non-zero curl transfer" >&2
    exit 1
fi
grep -q 'transport failed (curl exit 18; HTTP 200)' "$test_root/output.log"
if grep -q 'storage is ready' "$test_root/output.log"; then
    echo "owner probe reported readiness after a failed transfer" >&2
    exit 1
fi

echo "✅ Memoria owner transport contract passed"
