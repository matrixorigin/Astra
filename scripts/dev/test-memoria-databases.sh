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

failing_stub="$tmp_dir/failing-mysql"
cat >"$failing_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
    exit 0
fi
exit 42
STUB
chmod +x "$failing_stub"
if ASTRA_MYSQL_CLIENT="$failing_stub" "$script" >/dev/null 2>&1; then
    echo "expected database bootstrap failure to propagate" >&2
    exit 1
fi

if MEMORIA_DB_NAME='bad-name' "$script" >/dev/null 2>&1; then
    echo "expected invalid Memoria database name to fail" >&2
    exit 1
fi

echo "✅ Memoria database bootstrap contract passed"
