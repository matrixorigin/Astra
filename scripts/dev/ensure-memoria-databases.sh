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
"$repo_root/scripts/dev/mysql-client.sh" -e \
    "CREATE DATABASE IF NOT EXISTS \`$database_name\`; CREATE DATABASE IF NOT EXISTS \`$shared_database_name\`;"
