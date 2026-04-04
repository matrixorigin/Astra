#!/bin/bash
# Development environment initialization script (Rust-only)

set -e

PROJECT_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENV_FILE="$PROJECT_ROOT/.env"

if [ ! -f "$ENV_FILE" ]; then
    echo "❌ .env file not found. Create one first."
    exit 1
fi

echo "🔧 Initializing development environment..."
echo ""

update_or_add() {
    local key="$1"
    local value="$2"
    if grep -q "^${key}=" "$ENV_FILE"; then
        if [[ "$OSTYPE" == "darwin"* ]]; then
            sed -i '' "s#^${key}=.*#${key}=${value}#" "$ENV_FILE"
        else
            sed -i "s#^${key}=.*#${key}=${value}#" "$ENV_FILE"
        fi
    else
        echo "${key}=${value}" >> "$ENV_FILE"
    fi
}

if ! grep -q "^TOKEN_ENCRYPTION_KEY=" "$ENV_FILE" || grep -q "TOKEN_ENCRYPTION_KEY=.*CHANGE_ME" "$ENV_FILE"; then
    KEY="$(openssl rand -base64 32 | tr -d '\n')"
    update_or_add "TOKEN_ENCRYPTION_KEY" "$KEY"
    echo "✅ Generated TOKEN_ENCRYPTION_KEY"
else
    echo "✅ TOKEN_ENCRYPTION_KEY already configured"
fi

if ! grep -q "^JWT_SECRET_KEY=" "$ENV_FILE" || grep -q "JWT_SECRET_KEY=.*CHANGE_ME" "$ENV_FILE"; then
    JWT_KEY="$(openssl rand -hex 32)"
    update_or_add "JWT_SECRET_KEY" "$JWT_KEY"
    echo "✅ Generated JWT_SECRET_KEY"
else
    echo "✅ JWT_SECRET_KEY already configured"
fi

echo ""
echo "✅ Development environment initialized!"
echo ""
echo "Next steps:"
echo "  1. Start services:  make dev-start"
echo "  2. Run setup:       make dev-setup-demo"
echo "  3. Start chatting:  astra chat"
