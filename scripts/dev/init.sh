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

if ! grep -q "^ASTRA_TOKEN_ENCRYPTION_KEY=" "$ENV_FILE" || grep -q "ASTRA_TOKEN_ENCRYPTION_KEY=.*CHANGE_ME" "$ENV_FILE"; then
    KEY="$(openssl rand -base64 32 | tr -d '\n')"
    update_or_add "ASTRA_TOKEN_ENCRYPTION_KEY" "$KEY"
    echo "✅ Generated ASTRA_TOKEN_ENCRYPTION_KEY"
else
    echo "✅ ASTRA_TOKEN_ENCRYPTION_KEY already configured"
fi

if ! grep -q "^ASTRA_JWT_SECRET=" "$ENV_FILE" || grep -q "ASTRA_JWT_SECRET=.*CHANGE_ME" "$ENV_FILE"; then
    JWT_KEY="$(openssl rand -hex 32)"
    update_or_add "ASTRA_JWT_SECRET" "$JWT_KEY"
    echo "✅ Generated ASTRA_JWT_SECRET"
else
    echo "✅ ASTRA_JWT_SECRET already configured"
fi

if ! grep -q "^ASTRA_BRIDGE_SECRET=" "$ENV_FILE" || grep -Eq "^ASTRA_BRIDGE_SECRET=(|your-bridge-secret.*)$" "$ENV_FILE"; then
    BRIDGE_KEY="$(openssl rand -hex 32)"
    update_or_add "ASTRA_BRIDGE_SECRET" "$BRIDGE_KEY"
    echo "✅ Generated ASTRA_BRIDGE_SECRET"
else
    echo "✅ ASTRA_BRIDGE_SECRET already configured"
fi

# ── Optional: fast linker (mold) ──
CARGO_CONFIG="$PROJECT_ROOT/rust/.cargo/config.toml"
if [ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ]; then
    if command -v mold >/dev/null 2>&1 && command -v clang >/dev/null 2>&1; then
        if [ ! -f "$CARGO_CONFIG" ]; then
            mkdir -p "$(dirname "$CARGO_CONFIG")"
            cat > "$CARGO_CONFIG" <<'EOF'
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
EOF
            echo "✅ Configured mold linker (faster linking)"
        else
            echo "✅ Cargo config already exists, skipping mold setup"
        fi
    else
        echo "💡 Tip: install mold + clang for faster linking:"
        echo "   sudo apt install mold clang"
    fi
fi

echo ""
echo "✅ Development environment initialized!"
echo ""
echo "Next steps:"
echo "  1. Start services:  make dev-start"
echo "  2. Run setup:       make dev-setup-demo"
echo "  3. Start chatting:  astra chat"
