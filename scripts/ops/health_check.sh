#!/usr/bin/env bash
# Health check for all services

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

. "$(dirname "${BASH_SOURCE[0]}")/database_runtime.sh"
API_URL="$(env_resolve_value "$ops_env_file" API_URL 2>/dev/null || true)"
API_URL="${API_URL:-http://localhost:17001}"
ops_find_matrixone

echo "🏥 Health Check"
echo "==============="
echo ""

# Check API Server
echo -n "API Server (${API_URL}): "
health_no_proxy="${no_proxy:-${NO_PROXY:-}}"
if [[ "$health_no_proxy" != '*' ]]; then
    health_no_proxy="${health_no_proxy},localhost,127.0.0.1,::1"
fi
if curl --noproxy "$health_no_proxy" -sf "${API_URL}/health" > /dev/null 2>&1; then
    echo -e "${GREEN}✅ Healthy${NC}"
    API_STATUS=0
else
    echo -e "${RED}❌ Unhealthy${NC}"
    API_STATUS=1
fi

# Check MatrixOne
echo -n "MatrixOne (${DB_HOST}:${DB_PORT}): "
if ops_database_command mysql -e 'SELECT 1' >/dev/null 2>&1; then
    echo -e "${GREEN}✅ Connected${NC}"
    DB_STATUS=0
else
    echo -e "${RED}❌ Connection failed (check the selected endpoint and client tools)${NC}"
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
