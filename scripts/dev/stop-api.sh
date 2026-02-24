#!/bin/bash
# Stop API server

if [ -f api_server.pid ]; then
    kill $(cat api_server.pid) 2>/dev/null || true
    rm -f api_server.pid
else
    pkill -f "uvicorn api.main:app" 2>/dev/null || true
fi

echo "✅ API server stopped"
