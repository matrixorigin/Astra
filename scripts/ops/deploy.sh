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
echo "Running repository validation..."
if command -v cargo >/dev/null 2>&1 && [ -f rust/Cargo.toml ]; then
    make check && make test || {
        echo "❌ Validation failed. Aborting deployment."
        exit 1
    }
else
    echo "⚠️  cargo not available on this host; ensure artifacts were validated in CI before deployment."
fi

# 4. Database migration (if needed)
echo "Checking database..."
# Add migration logic here if needed

# 5. Start API server
echo "Starting API server..."
echo "  Host: ${API_HOST:-0.0.0.0}"
echo "  Port: ${API_PORT:-8000}"
echo "  Workers: ${API_WORKERS:-4}"

RUST_API_ADDR="${API_HOST:-0.0.0.0}:${API_PORT:-8000}" \
    astra-server

echo "✅ Deployment complete!"
