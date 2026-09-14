#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo_root/scripts/dev/ensure-memoria-databases.sh"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

stub="$tmp_dir/mysql"
cat >"$stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
    exit 0
fi
if [[ "${1:-}" == "--protocol=TCP" ]]; then
    if [[ " $* " == *" -e SELECT 1 "* ]]; then
        exit 0
    fi
    printf '%s\n' "$*" >"${MYSQL_STUB_OUTPUT:?}"
fi
STUB
chmod +x "$stub"

output="$tmp_dir/sql"
ASTRA_MYSQL_CLIENT="$stub" \
MYSQL_STUB_OUTPUT="$output" \
MEMORIA_DB_NAME=memoria_test \
ASTRA_MYSQL_TLS_MODE=auto \
    "$script" >/dev/null

grep -Fq 'CREATE DATABASE IF NOT EXISTS `memoria_test`' "$output"
grep -Fq 'CREATE DATABASE IF NOT EXISTS `memoria_test_shared`' "$output"

retry_stub="$tmp_dir/retry-mysql"
retry_count="$tmp_dir/retry-count"
: >"$retry_count"
cat >"$retry_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
    exit 0
fi
if [[ " $* " == *" -e SELECT 1 "* ]]; then
    exit 0
fi
count="$(cat "${MYSQL_RETRY_COUNT:?}")"
if [[ -z "$count" ]]; then
    printf '1' >"${MYSQL_RETRY_COUNT:?}"
    exit 42
fi
printf '%s\n' "$*" >"${MYSQL_STUB_OUTPUT:?}"
STUB
chmod +x "$retry_stub"
ASTRA_MYSQL_CLIENT="$retry_stub" \
MYSQL_RETRY_COUNT="$retry_count" \
MYSQL_STUB_OUTPUT="$output" \
MEMORIA_DB_NAME=memoria_retry \
MEMORIA_DB_READY_RETRIES=2 \
MEMORIA_DB_READY_RETRY_DELAY_SECONDS=0 \
ASTRA_MYSQL_TLS_MODE=auto \
    "$script" >/dev/null
grep -Fq 'CREATE DATABASE IF NOT EXISTS `memoria_retry`' "$output"

failing_stub="$tmp_dir/failing-mysql"
cat >"$failing_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
    exit 0
fi
echo 'fatal mysql stub' >&2
exit 42
STUB
chmod +x "$failing_stub"
failure_output="$tmp_dir/failure.log"
if ASTRA_MYSQL_CLIENT="$failing_stub" \
    MEMORIA_DB_READY_RETRIES=2 \
    MEMORIA_DB_READY_RETRY_DELAY_SECONDS=0 \
    "$script" >"$failure_output" 2>&1; then
    echo "expected database bootstrap failure to propagate" >&2
    exit 1
fi
grep -Fq 'fatal mysql stub' "$failure_output"
grep -Fq 'MatrixOne SQL was not ready after 2 attempt(s)' "$failure_output"

for invalid_retries in 00 08; do
    if MEMORIA_DB_READY_RETRIES="$invalid_retries" "$script" >/dev/null 2>&1; then
        echo "expected invalid retry count to fail: $invalid_retries" >&2
        exit 1
    fi
done

if MEMORIA_DB_NAME='bad-name' "$script" >/dev/null 2>&1; then
    echo "expected invalid Memoria database name to fail" >&2
    exit 1
fi

echo "✅ Memoria database bootstrap contract passed"
