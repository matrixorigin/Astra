#!/usr/bin/env bash
# Health check for all services

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration
API_URL="${API_URL:-http://localhost:17001}"

if [ -f .env ]; then
    set -a; source .env; set +a
fi

if [ -z "${MATRIXONE_PASSWORD:-}" ]; then
    echo -e "${RED}❌ MATRIXONE_PASSWORD is required (set it explicitly or source .env)${NC}"
    exit 1
fi

DB_HOST="${MATRIXONE_HOST:-localhost}"
DB_PORT="${MATRIXONE_PORT:-6001}"
DB_USER="${MATRIXONE_USER:-root}"
DB_PASSWORD="${MATRIXONE_PASSWORD}"

matrixone_container_ids=""
if command -v docker >/dev/null 2>&1 && \
   { [ "${DB_HOST}" = "localhost" ] || [ "${DB_HOST}" = "127.0.0.1" ]; }; then
    matrixone_container_ids="$(docker ps \
        --filter label=com.docker.compose.service=matrixone \
        --format '{{.ID}}' 2>/dev/null || true)"
fi
matrixone_container_count="$(printf '%s\n' "${matrixone_container_ids}" | sed '/^$/d' | wc -l | tr -d ' ')"

echo "🏥 Health Check"
echo "==============="
echo ""

# Check API Server
echo -n "API Server (${API_URL}): "
if curl -sf "${API_URL}/health" > /dev/null 2>&1; then
    echo -e "${GREEN}✅ Healthy${NC}"
    API_STATUS=0
else
    echo -e "${RED}❌ Unhealthy${NC}"
    API_STATUS=1
fi

# Check MatrixOne
echo -n "MatrixOne (${DB_HOST}:${DB_PORT}): "
if [ "${matrixone_container_count}" -eq 1 ]; then
    if MYSQL_PWD="${DB_PASSWORD}" docker exec -e MYSQL_PWD "${matrixone_container_ids}" \
        mysql -h127.0.0.1 -P6001 -u"${DB_USER}" -e "SELECT 1" > /dev/null 2>&1; then
        echo -e "${GREEN}✅ Connected${NC}"
        DB_STATUS=0
    else
        echo -e "${RED}❌ Connection failed${NC}"
        DB_STATUS=1
    fi
elif [ "${matrixone_container_count}" -gt 1 ]; then
    echo -e "${RED}❌ Multiple local MatrixOne Compose containers found; stop unused stacks${NC}"
    DB_STATUS=1
elif command -v mysql >/dev/null 2>&1; then
    if MYSQL_PWD="${DB_PASSWORD}" mysql -h"${DB_HOST}" -P"${DB_PORT}" \
        -u"${DB_USER}" -e "SELECT 1" > /dev/null 2>&1; then
        echo -e "${GREEN}✅ Connected${NC}"
        DB_STATUS=0
    else
        echo -e "${RED}❌ Connection failed${NC}"
        DB_STATUS=1
    fi
else
    echo -e "${YELLOW}⚠️  Cannot check (mysql client not found)${NC}"
    DB_STATUS=1
fi

echo ""

# Overall status
TOTAL_STATUS=$((API_STATUS + DB_STATUS))

if [ $TOTAL_STATUS -eq 0 ]; then
    echo -e "${GREEN}✅ All services healthy${NC}"
    exit 0
else
    echo -e "${RED}❌ Some services are unhealthy${NC}"
    echo ""
    echo "Troubleshooting:"
    echo "  • Check logs: make dev-api-logs, make dev-deps-logs"
    echo "  • Check status: make dev-status"
    echo "  • Restart services: make dev-restart"
    exit 1
fi
