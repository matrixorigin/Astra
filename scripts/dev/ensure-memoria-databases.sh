#!/usr/bin/env bash
# Ensure the databases required by Memoria's multi-database deployment exist.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
database_name="${MEMORIA_DB_NAME:-memoria}"
shared_database_name="${database_name}_shared"

for name in "$database_name" "$shared_database_name"; do
    case "$name" in
        ''|*[!A-Za-z0-9_]* )
            echo "Memoria database names must contain only letters, numbers, and underscores: $name" >&2
            exit 2
            ;;
    esac
done

echo "Ensuring Memoria databases: $database_name, $shared_database_name"
database_sql="CREATE DATABASE IF NOT EXISTS \`$database_name\`; CREATE DATABASE IF NOT EXISTS \`$shared_database_name\`;"
max_attempts="${MEMORIA_DB_READY_RETRIES:-90}"
retry_delay="${MEMORIA_DB_READY_RETRY_DELAY_SECONDS:-2}"

case "$max_attempts" in
    ''|0|0*|*[!0-9]*)
        echo "MEMORIA_DB_READY_RETRIES must be a positive integer: $max_attempts" >&2
        exit 2
        ;;
esac
case "$retry_delay" in
    ''|*[!0-9]*)
        echo "MEMORIA_DB_READY_RETRY_DELAY_SECONDS must be a non-negative integer: $retry_delay" >&2
        exit 2
        ;;
esac

last_output=""
for attempt in $(seq 1 "$max_attempts"); do
    if last_output=$("$repo_root/scripts/dev/mysql-client.sh" -e "$database_sql" 2>&1); then
        printf '%s\n' "$last_output"
        exit 0
    fi

    if [[ "$attempt" -eq "$max_attempts" ]]; then
        printf '%s\n' "$last_output" >&2
        echo "❌ MatrixOne SQL was not ready after $max_attempts attempt(s)" >&2
        exit 1
    fi

    echo "  Waiting for MatrixOne SQL readiness... ($attempt/$max_attempts)"
    sleep "$retry_delay"
done

echo "❌ MatrixOne SQL readiness retry loop ended unexpectedly" >&2
exit 1
