#!/usr/bin/env bash
# Backup MatrixOne database

set -euo pipefail

# Configuration
BACKUP_DIR="${BACKUP_DIR:-./backups}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/astra_backup_${TIMESTAMP}.sql"

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

echo "🔄 Starting database backup..."
echo "   Database: ${DB_NAME}"
echo "   Host: ${DB_HOST}:${DB_PORT}"
echo "   Backup file: ${BACKUP_FILE}"

# Create backup directory
mkdir -p "${BACKUP_DIR}"
TEMP_BACKUP="${BACKUP_FILE}.tmp"
trap 'rm -f "${TEMP_BACKUP}"' EXIT

matrixone_container_ids=""
if command -v docker >/dev/null 2>&1 && \
   { [ "${DB_HOST}" = "localhost" ] || [ "${DB_HOST}" = "127.0.0.1" ]; }; then
    matrixone_container_ids="$(docker ps \
        --filter label=com.docker.compose.service=matrixone \
        --format '{{.ID}}' 2>/dev/null || true)"
fi
matrixone_container_count="$(printf '%s\n' "${matrixone_container_ids}" | sed '/^$/d' | wc -l | tr -d ' ')"

# Perform backup
if [ "${matrixone_container_count}" -eq 1 ]; then
    echo "   Using local Compose MatrixOne service..."
    MYSQL_PWD="${DB_PASSWORD}" docker exec -e MYSQL_PWD "${matrixone_container_ids}" \
        mysqldump -h127.0.0.1 -P6001 -u"${DB_USER}" "${DB_NAME}" > "${TEMP_BACKUP}"
elif [ "${matrixone_container_count}" -gt 1 ]; then
    echo "Error: multiple local MatrixOne Compose containers found; stop unused stacks." >&2
    exit 1
else
    if ! command -v mysqldump >/dev/null 2>&1; then
        echo "Error: mysqldump is required when no local Compose MatrixOne service is available." >&2
        exit 1
    fi
    echo "   Using local mysqldump..."
    MYSQL_PWD="${DB_PASSWORD}" mysqldump \
        -h"${DB_HOST}" -P"${DB_PORT}" -u"${DB_USER}" \
        "${DB_NAME}" > "${TEMP_BACKUP}"
fi

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
echo "   ./scripts/ops/restore.sh ${BACKUP_FILE}"
