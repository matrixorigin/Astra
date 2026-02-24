#!/bin/bash
# Start API server

set -e

PID_FILE="api_server.pid"
LOG_FILE="api_server.log"

echo "Starting API server..."

# Check if already running
if [ -f "$PID_FILE" ] && kill -0 $(cat "$PID_FILE") 2>/dev/null; then
    echo "⚠️  API server already running (PID: $(cat $PID_FILE))"
    exit 0
fi

# Clean up old process
rm -f "$PID_FILE"
pkill -f "python -m uvicorn api.main:app" 2>/dev/null || true
sleep 1

# Wait for database to be ready (retry up to 30 seconds)
echo "Waiting for database..."
for i in {1..15}; do
    if python3 -c "import pymysql; pymysql.connect(host='127.0.0.1', port=6001, user='root', password='111')" >/dev/null 2>&1; then
        echo "✅ Database ready"
        break
    fi
    if [ $i -lt 15 ]; then
        echo "  Retrying... ($i/15)"
        sleep 2
    else
        echo "❌ Database not responding after 30 seconds"
        exit 1
    fi
done

# Load .env into environment (pydantic-settings reads .env into Settings objects,
# but modules like encryption.py use os.getenv directly)
if [ -f .env ]; then
    set -a; source .env; set +a
fi

# Start server
NO_PROXY=localhost,127.0.0.1 python -m uvicorn api.main:app --port 8000 > "$LOG_FILE" 2>&1 &
PID=$!
echo $PID > "$PID_FILE"

# Wait and check
sleep 2
if kill -0 $PID 2>/dev/null; then
    echo "✅ API server started (PID: $PID)"
else
    echo "❌ API server failed to start"
    echo ""
    echo "Troubleshooting:"
    echo "  1. Check if port 8000 is in use: lsof -i :8000"
    echo "  2. View error log: tail -50 $LOG_FILE"
    echo "  3. Kill stuck processes: pkill -f 'python -m uvicorn'"
    echo "  4. Check database: make dev-status"
    exit 1
fi
