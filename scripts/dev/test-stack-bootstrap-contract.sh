#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-stack-bootstrap.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

stack_env="$test_root/stack.env"
cat >"$stack_env" <<'ENV'
ASTRA_STACK_NAME=contract-stack
ASTRA_API_PORT=17001
ASTRA_BIND_ADDRESS=127.0.0.1
MATRIXONE_PORT=26001
MEMORIA_PORT=8100
MATRIXONE_USER=root
MATRIXONE_PASSWORD=111
MEMORIA_DB_NAME=memoria
MEMORIA_EMBEDDING_PROVIDER=mock
ASTRA_JWT_SECRET=contract-jwt
ASTRA_TOKEN_ENCRYPTION_KEY=contract-token
ASTRA_RUNTIME_ROOT_SECRET=contract-root
MEMORIA_MASTER_KEY=contract-memory
ENV

compose_log="$test_root/compose.log"
event_log="$test_root/events.log"
compose_stub="$test_root/compose"
cat >"$compose_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${COMPOSE_LOG:?}"
printf 'compose:%s\n' "$*" >>"${EVENT_LOG:?}"
exit 0
STUB
chmod +x "$compose_stub"

mysql_stub="$test_root/mysql"
cat >"$mysql_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
    exit 0
fi
if [[ " $* " == *" -e SELECT 1 "* ]]; then
    exit 0
fi
exit 42
STUB
chmod +x "$mysql_stub"

if COMPOSE_LOG="$compose_log" \
    EVENT_LOG="$event_log" \
    ASTRA_MYSQL_CLIENT="$mysql_stub" \
    ASTRA_MYSQL_TLS_MODE=auto \
    MEMORIA_DB_READY_RETRIES=1 \
    MEMORIA_DB_READY_RETRY_DELAY_SECONDS=0 \
    make --no-print-directory -s -C "$repo_root" \
        STACK_ENV="$stack_env" STACK_COMPOSE="$compose_stub" stack-up \
        >"$test_root/failure.log" 2>&1; then
    echo "expected stack-up to fail when Memoria database bootstrap fails" >&2
    exit 1
fi
grep -Eq 'up -d[[:space:]]+--wait --wait-timeout 180 matrixone' "$compose_log"
if grep -Eq 'memoria api' "$compose_log"; then
    echo "stack-up started Memoria/API after bootstrap failure" >&2
    exit 1
fi

mysql_ok_stub="$test_root/mysql-ok"
cat >"$mysql_ok_stub" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "--help" ]]; then
    echo '--ssl-mode'
elif [[ " $* " != *" -e SELECT 1 "* ]]; then
    printf 'mysql:%s\n' "$*" >>"${EVENT_LOG:?}"
fi
exit 0
STUB
chmod +x "$mysql_ok_stub"
: >"$compose_log"
: >"$event_log"
COMPOSE_LOG="$compose_log" \
EVENT_LOG="$event_log" \
ASTRA_MYSQL_CLIENT="$mysql_ok_stub" \
ASTRA_MYSQL_TLS_MODE=auto \
MEMORIA_DB_READY_RETRIES=1 \
MEMORIA_DB_READY_RETRY_DELAY_SECONDS=0 \
    make --no-print-directory -s -C "$repo_root" \
        STACK_ENV="$stack_env" STACK_COMPOSE="$compose_stub" stack-up \
        >"$test_root/success.log" 2>&1
grep -Eq 'up -d[[:space:]]+--wait --wait-timeout 180 matrixone' "$compose_log"
grep -Eq 'up -d[[:space:]]+--wait --wait-timeout 180 memoria api' "$compose_log"
matrixone_line="$(grep -n '^compose:.*matrixone$' "$event_log" | head -n 1 | cut -d: -f1)"
bootstrap_line="$(grep -n '^mysql:.*CREATE DATABASE' "$event_log" | head -n 1 | cut -d: -f1)"
memoria_line="$(grep -n '^compose:.*memoria api$' "$event_log" | head -n 1 | cut -d: -f1)"
if [[ -z "$matrixone_line" || -z "$bootstrap_line" || -z "$memoria_line" ]] ||
    ((matrixone_line >= bootstrap_line || bootstrap_line >= memoria_line)); then
    echo "stack-up did not preserve MatrixOne -> bootstrap -> Memoria/API ordering" >&2
    exit 1
fi

echo "✅ Stack Memoria bootstrap contract passed"
