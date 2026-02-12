#!/bin/bash
# Production deployment script

set -e

echo "🚀 Deploying Agent Engine API..."

# 1. Check environment
if [ ! -f .env.production ]; then
    echo "❌ Error: .env.production not found"
    echo "   Copy .env.production.example and configure it"
    exit 1
fi

# 2. Load environment
export $(cat .env.production | grep -v '^#' | xargs)

# 3. Run tests
echo "Running tests..."
python -m pytest tests/ -q || {
    echo "❌ Tests failed. Aborting deployment."
    exit 1
}

# 4. Database migration (if needed)
echo "Checking database..."
# Add migration logic here if needed

# 5. Start API server
echo "Starting API server..."
echo "  Host: ${API_HOST:-0.0.0.0}"
echo "  Port: ${API_PORT:-8000}"
echo "  Workers: ${API_WORKERS:-4}"

uvicorn api.main:app \
    --host ${API_HOST:-0.0.0.0} \
    --port ${API_PORT:-8000} \
    --workers ${API_WORKERS:-4} \
    --log-level ${LOG_LEVEL:-info}

echo "✅ Deployment complete!"
