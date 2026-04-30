#!/bin/bash
# Health check for all services

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration
API_URL="${API_URL:-http://localhost:8000}"

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
if command -v docker &> /dev/null && docker ps | grep -q matrixone; then
    if docker exec matrixone mysql -h127.0.0.1 -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} -e "SELECT 1" > /dev/null 2>&1; then
        echo -e "${GREEN}✅ Connected${NC}"
        DB_STATUS=0
    else
        echo -e "${RED}❌ Connection failed${NC}"
        DB_STATUS=1
    fi
elif command -v mysql &> /dev/null; then
    if mysql -h${DB_HOST} -P${DB_PORT} -u${DB_USER} -p${DB_PASSWORD} -e "SELECT 1" > /dev/null 2>&1; then
        echo -e "${GREEN}✅ Connected${NC}"
        DB_STATUS=0
    else
        echo -e "${RED}❌ Connection failed${NC}"
        DB_STATUS=1
    fi
else
    echo -e "${YELLOW}⚠️  Cannot check (mysql client not found)${NC}"
    DB_STATUS=0
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
