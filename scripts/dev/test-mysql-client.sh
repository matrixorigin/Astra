#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
client_script="$repo_root/scripts/dev/mysql-client.sh"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

stub="$tmp_dir/mysql-stub"
printf '%s\n' \
    '#!/usr/bin/env bash' \
    'if [[ "$*" == *"--help"* ]]; then' \
    '  [[ "${MYSQL_STUB_SUPPORTS_SSL_MODE:-}" == "1" ]] && printf "%s\\n" "  --ssl-mode=name"' \
    '  [[ "${MYSQL_STUB_SUPPORTS_SKIP_SSL:-}" == "1" ]] && printf "%s\\n" "  --skip-ssl"' \
    '  exit 0' \
    'fi' \
    'count_file="${ARGS_FILE}.count"' \
    'count=$(cat "$count_file" 2>/dev/null || printf "0")' \
    'count=$((count + 1))' \
    'printf "%s" "$count" >"$count_file"' \
    'printf '\''%s\n'\'' "$@" >"${ARGS_FILE}.${count}"' \
    'if [[ "${MYSQL_STUB_REJECT_PREFERRED:-}" == "1" && "$*" == *"--ssl-mode=PREFERRED"* ]]; then exit 1; fi' \
    'if [[ "${MYSQL_STUB_REJECT_UNSET_TLS:-}" == "1" && "$*" != *"--skip-ssl"* && "$*" != *"--ssl-mode="* ]]; then exit 1; fi' \
    >"$stub"
chmod +x "$stub"

assert_args() {
    local capture="$1"
    shift
    local actual expected
    actual=$(<"$capture")
    expected=$(printf '%s\n' "$@")
    if [[ "$actual" != "$expected" ]]; then
        printf 'unexpected mysql argv:\n  got:\n%s\n  want:\n%s\n' "$actual" "$expected" >&2
        exit 1
    fi
}

base_args=(--protocol=TCP -hdb.example -P6601 -uuser -psecret)

ARGS_FILE="$tmp_dir/default.args" \
MYSQL_STUB_SUPPORTS_SSL_MODE=1 \
ASTRA_MYSQL_CLIENT="$stub" \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'SELECT 1'
assert_args "$tmp_dir/default.args.2" "${base_args[@]}" --ssl-mode=PREFERRED -e 'SELECT 1'

ARGS_FILE="$tmp_dir/legacy-default.args" \
ASTRA_MYSQL_CLIENT="$stub" \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'SELECT 1'
assert_args "$tmp_dir/legacy-default.args.2" "${base_args[@]}" -e 'SELECT 1'

ARGS_FILE="$tmp_dir/disabled.args" \
ASTRA_MYSQL_CLIENT="$stub" \
ASTRA_MYSQL_TLS_MODE=disabled \
MYSQL_STUB_SUPPORTS_SSL_MODE=1 \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'SELECT 1'
assert_args "$tmp_dir/disabled.args.1" "${base_args[@]}" --ssl-mode=DISABLED -e 'SELECT 1'

ARGS_FILE="$tmp_dir/required.args" \
ASTRA_MYSQL_CLIENT="$stub" \
ASTRA_MYSQL_TLS_MODE=required \
MYSQL_STUB_SUPPORTS_SSL_MODE=1 \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'SELECT 1'
assert_args "$tmp_dir/required.args.1" "${base_args[@]}" --ssl-mode=REQUIRED -e 'SELECT 1'

# A client can advertise modern TLS flags while an endpoint (or ambient client
# configuration) still rejects the TLS-preferred connection. Auto mode must
# establish its deliberate plaintext policy with a harmless probe and execute
# the caller's SQL exactly once.
ARGS_FILE="$tmp_dir/fallback.args" \
MYSQL_STUB_SUPPORTS_SSL_MODE=1 \
MYSQL_STUB_REJECT_PREFERRED=1 \
ASTRA_MYSQL_CLIENT="$stub" \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'DROP DATABASE test_db'
assert_args "$tmp_dir/fallback.args.3" "${base_args[@]}" --ssl-mode=DISABLED -e 'DROP DATABASE test_db'
if [[ $(<"$tmp_dir/fallback.args.count") != 3 ]]; then
    echo "auto TLS fallback must make two probes and one caller request" >&2
    exit 1
fi

# Legacy clients have no portable negotiation switch. When their ambient
# defaults cannot connect, auto mode may use --skip-ssl only after proving the
# explicit plaintext probe works.
ARGS_FILE="$tmp_dir/legacy-fallback.args" \
MYSQL_STUB_SUPPORTS_SKIP_SSL=1 \
MYSQL_STUB_REJECT_UNSET_TLS=1 \
ASTRA_MYSQL_CLIENT="$stub" \
MATRIXONE_HOST=db.example \
MATRIXONE_PORT=6601 \
MATRIXONE_USER=user \
MATRIXONE_PASSWORD=secret \
"$client_script" -e 'SELECT 1'
assert_args "$tmp_dir/legacy-fallback.args.3" "${base_args[@]}" --skip-ssl -e 'SELECT 1'

if ARGS_FILE="$tmp_dir/unsupported.args" \
    ASTRA_MYSQL_CLIENT="$stub" \
    ASTRA_MYSQL_TLS_MODE=disabled \
    MATRIXONE_HOST=db.example \
    MATRIXONE_PORT=6601 \
    MATRIXONE_USER=user \
    MATRIXONE_PASSWORD=secret \
    "$client_script" -e 'SELECT 1' >/dev/null 2>&1; then
    echo "expected disabled TLS mode to reject a client with no TLS controls" >&2
    exit 1
fi

echo "mysql client TLS option tests passed"
