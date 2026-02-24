#!/bin/bash
# Backup MatrixOne database

set -e

# Configuration
BACKUP_DIR="${BACKUP_DIR:-./backups}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/mo_agent_backup_${TIMESTAMP}.sql"

# Database connection
DB_HOST="${MATRIXONE_HOST:-localhost}"
DB_PORT="${MATRIXONE_PORT:-6001}"
DB_USER="${MATRIXONE_USER:-root}"
DB_PASSWORD="${MATRIXONE_PASSWORD:-111}"
DB_NAME="${MATRIXONE_DATABASE:-mo_agent}"

echo "🔄 Starting database backup..."
echo "   Database: ${DB_NAME}"
echo "   Host: ${DB_HOST}:${DB_PORT}"
echo "   Backup file: ${BACKUP_FILE}"

# Create backup directory
mkdir -p "${BACKUP_DIR}"

# Perform backup
if command -v docker &> /dev/null && docker ps | grep -q matrixone; then
    # Use Docker if MatrixOne is running in container
    echo "   Using Docker container..."
    docker exec matrixone mysqldump \
        -h127.0.0.1 -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} \
        ${DB_NAME} > "${BACKUP_FILE}"
else
    # Use local mysqldump
    echo "   Using local mysqldump..."
    mysqldump \
        -h${DB_HOST} -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} \
        ${DB_NAME} > "${BACKUP_FILE}"
fi

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
if [ "${CLEANUP_OLD_BACKUPS:-true}" = "true" ]; then
    echo "🧹 Cleaning old backups (keeping last 7 days)..."
    find "${BACKUP_DIR}" -name "mo_agent_backup_*.sql.gz" -mtime +7 -delete
    echo "✅ Cleanup completed"
fi

echo ""
echo "📋 To restore this backup, run:"
echo "   ./scripts/ops/restore.sh ${BACKUP_FILE}"
