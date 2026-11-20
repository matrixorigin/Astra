#!/usr/bin/env bash
# Backup MatrixOne database

set -euo pipefail

# Configuration
BACKUP_DIR="${BACKUP_DIR:-./backups}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/astra_backup_${TIMESTAMP}.sql"

. "$(dirname "${BASH_SOURCE[0]}")/database_runtime.sh"

echo "🔄 Starting database backup..."
echo "   Database: ${DB_NAME}"
echo "   Host: ${DB_HOST}:${DB_PORT}"
echo "   Backup file: ${BACKUP_FILE}"

# Create backup directory
mkdir -p "${BACKUP_DIR}"
TEMP_BACKUP="${BACKUP_FILE}.tmp"
trap 'rm -f "${TEMP_BACKUP}"' EXIT

ops_find_matrixone

# Write atomically; preserve native client failures without publishing a partial dump.
ops_database_command mysqldump "$DB_NAME" > "$TEMP_BACKUP"

mv "${TEMP_BACKUP}" "${BACKUP_FILE}"

# Compress backup
echo "🗜️  Compressing backup..."
gzip "${BACKUP_FILE}"
BACKUP_FILE="${BACKUP_FILE}.gz"

# Get file size
BACKUP_SIZE=$(du -h "${BACKUP_FILE}" | cut -f1)

echo "✅ Backup completed successfully!"
echo "   File: ${BACKUP_FILE}"
echo "   Size: ${BACKUP_SIZE}"

# Optional: Upload to S3 (uncomment if needed)
# if [ -n "${AWS_S3_BUCKET}" ]; then
#     echo "☁️  Uploading to S3..."
#     aws s3 cp "${BACKUP_FILE}" "s3://${AWS_S3_BUCKET}/backups/"
#     echo "✅ Uploaded to S3"
# fi

# Optional: Clean old backups (keep last 7 days)
if [ "${CLEANUP_OLD_BACKUPS:-false}" = "true" ]; then
    echo "🧹 Cleaning old backups (keeping last 7 days)..."
    find "${BACKUP_DIR}" -name "astra_backup_*.sql.gz" -mtime +7 -delete
    echo "✅ Cleanup completed"
fi

echo ""
echo "📋 To restore this backup, run:"
printf '   ASTRA_CONTAINER_ENGINE=%q ASTRA_OPS_ENV_FILE=%q ./scripts/ops/restore.sh %q\n' \
    "$container_engine" "$ops_env_file" "$BACKUP_FILE"
