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

. "$(dirname "${BASH_SOURCE[0]}")/database_runtime.sh"

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

ops_find_matrixone

ops_database_command mysql "$DB_NAME" < "$RESTORE_FILE"

echo "✅ Restore completed successfully!"
echo ""
echo "📋 Next steps:"
echo "   1. Verify data: make dev-status"
echo "   2. Test API: curl http://localhost:17001/health"
echo "   3. Restart services if needed: make dev-restart"
