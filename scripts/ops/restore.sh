#!/bin/bash
# Restore MatrixOne database from backup

set -e

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

# Database connection
DB_HOST="${MATRIXONE_HOST:-localhost}"
DB_PORT="${MATRIXONE_PORT:-6001}"
DB_USER="${MATRIXONE_USER:-root}"
DB_PASSWORD="${MATRIXONE_PASSWORD:-111}"
DB_NAME="${MATRIXONE_DATABASE:-astra}"

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
if [[ "${BACKUP_FILE}" == *.gz ]]; then
    echo "📦 Decompressing backup..."
    TEMP_FILE="/tmp/astra_restore_$$.sql"
    gunzip -c "${BACKUP_FILE}" > "${TEMP_FILE}"
    RESTORE_FILE="${TEMP_FILE}"
else
    RESTORE_FILE="${BACKUP_FILE}"
fi

# Perform restore
if command -v docker &> /dev/null && docker ps | grep -q matrixone; then
    # Use Docker if MatrixOne is running in container
    echo "   Using Docker container..."
    docker exec -i matrixone mysql \
        -h127.0.0.1 -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} \
        ${DB_NAME} < "${RESTORE_FILE}"
else
    # Use local mysql
    echo "   Using local mysql..."
    mysql \
        -h${DB_HOST} -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} \
        ${DB_NAME} < "${RESTORE_FILE}"
fi

# Cleanup temp file
if [ -n "${TEMP_FILE}" ]; then
    rm -f "${TEMP_FILE}"
fi

echo "✅ Restore completed successfully!"
echo ""
echo "📋 Next steps:"
echo "   1. Verify data: make dev-status"
echo "   2. Test API: curl http://localhost:8000/health"
echo "   3. Restart services if needed: make dev-restart"
