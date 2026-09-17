#!/usr/bin/env bash
# Restore MatrixOne database from backup

set -euo pipefail

# Check arguments
if [ $# -eq 0 ]; then
    echo "❌ Error: Backup file not specified"
    echo ""
    echo "Usage: $0 <backup_file>"
    echo ""
    echo "Example:"
    echo "   $0 backups/astra_backup_20260224_091500.sql.gz"
    echo "   $0 backups/astra_backup_20260224_091500.sql"
    exit 1
fi

BACKUP_FILE="$1"

# Check if backup file exists
if [ ! -f "${BACKUP_FILE}" ]; then
    echo "❌ Error: Backup file not found: ${BACKUP_FILE}"
    exit 1
fi

# Load .env if present so local dev scripts pick up the same explicit secrets.
if [ -f .env ]; then
    set -a; source .env; set +a
fi

if [ -z "${MATRIXONE_PASSWORD:-}" ]; then
    echo "❌ Error: MATRIXONE_PASSWORD is required (set it explicitly or source .env)"
    exit 1
fi

# Database connection
DB_HOST="${MATRIXONE_HOST:-localhost}"
DB_PORT="${MATRIXONE_PORT:-6001}"
DB_USER="${MATRIXONE_USER:-root}"
DB_PASSWORD="${MATRIXONE_PASSWORD}"
DB_NAME="${ASTRA_DATABASE:-astra_runtime}"

echo "⚠️  WARNING: This will replace all data in database '${DB_NAME}'"
echo "   Host: ${DB_HOST}:${DB_PORT}"
echo "   Backup file: ${BACKUP_FILE}"
echo ""
read -p "Are you sure you want to continue? (yes/no): " -r
echo ""

if [[ ! $REPLY =~ ^[Yy][Ee][Ss]$ ]]; then
    echo "❌ Restore cancelled"
    exit 0
fi

echo "🔄 Starting database restore..."

# Decompress if needed
TEMP_FILE=""
trap 'if [ -n "${TEMP_FILE}" ]; then rm -f "${TEMP_FILE}"; fi' EXIT
if [[ "${BACKUP_FILE}" == *.gz ]]; then
    echo "📦 Decompressing backup..."
    TEMP_FILE="$(mktemp "${TMPDIR:-/tmp}/astra-restore.XXXXXX.sql")"
    gunzip -c "${BACKUP_FILE}" > "${TEMP_FILE}"
    RESTORE_FILE="${TEMP_FILE}"
else
    RESTORE_FILE="${BACKUP_FILE}"
fi

matrixone_container_ids=""
if command -v docker >/dev/null 2>&1 && \
   { [ "${DB_HOST}" = "localhost" ] || [ "${DB_HOST}" = "127.0.0.1" ]; }; then
    matrixone_container_ids="$(docker ps \
        --filter label=com.docker.compose.service=matrixone \
        --format '{{.ID}}' 2>/dev/null || true)"
fi
matrixone_container_count="$(printf '%s\n' "${matrixone_container_ids}" | sed '/^$/d' | wc -l | tr -d ' ')"

# Perform restore
if [ "${matrixone_container_count}" -eq 1 ]; then
    echo "   Using local Compose MatrixOne service..."
    MYSQL_PWD="${DB_PASSWORD}" docker exec -e MYSQL_PWD -i "${matrixone_container_ids}" \
        mysql -h127.0.0.1 -P6001 -u"${DB_USER}" "${DB_NAME}" < "${RESTORE_FILE}"
elif [ "${matrixone_container_count}" -gt 1 ]; then
    echo "Error: multiple local MatrixOne Compose containers found; stop unused stacks." >&2
    exit 1
else
    if ! command -v mysql >/dev/null 2>&1; then
        echo "Error: mysql is required when no local Compose MatrixOne service is available." >&2
        exit 1
    fi
    echo "   Using local mysql..."
    MYSQL_PWD="${DB_PASSWORD}" mysql \
        -h"${DB_HOST}" -P"${DB_PORT}" -u"${DB_USER}" \
        "${DB_NAME}" < "${RESTORE_FILE}"
fi

echo "✅ Restore completed successfully!"
echo ""
echo "📋 Next steps:"
echo "   1. Verify data: make dev-status"
echo "   2. Test API: curl http://localhost:17001/health"
echo "   3. Restart services if needed: make dev-restart"
