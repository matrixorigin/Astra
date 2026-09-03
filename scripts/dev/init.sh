#!/bin/bash
# Development environment initialization script (Rust-only)

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENV_FILE="$PROJECT_ROOT/.env"
. "$PROJECT_ROOT/scripts/lib/env_file.sh"

if [ ! -f "$ENV_FILE" ]; then
    echo "❌ .env file not found. Create one first."
    exit 1
fi

echo "🔧 Initializing development environment..."
echo ""

update_or_add() {
    local key="$1"
    local value="$2"
    if grep -Eq "^[[:space:]]*${key}[[:space:]]*=" "$ENV_FILE"; then
        if [[ "$OSTYPE" == "darwin"* ]]; then
            sed -i '' "s#^[[:space:]]*${key}[[:space:]]*=.*#${key}=${value}#" "$ENV_FILE"
        else
            sed -i "s#^[[:space:]]*${key}[[:space:]]*=.*#${key}=${value}#" "$ENV_FILE"
        fi
    else
        echo "${key}=${value}" >> "$ENV_FILE"
    fi
}

needs_generated_secret() {
    local key="$1"
    local value
    value="$(env_file_read "$ENV_FILE" "$key" 2>/dev/null || true)"
    env_value_is_placeholder "$value"
}

generate_secret() {
    if ! command -v openssl >/dev/null 2>&1; then
        echo "❌ openssl is required to generate local secrets" >&2
        return 1
    fi
    openssl rand "$@"
}

if needs_generated_secret "ASTRA_TOKEN_ENCRYPTION_KEY"; then
    KEY="$(generate_secret -base64 32)"
    update_or_add "ASTRA_TOKEN_ENCRYPTION_KEY" "$KEY"
    echo "✅ Generated ASTRA_TOKEN_ENCRYPTION_KEY"
else
    echo "✅ ASTRA_TOKEN_ENCRYPTION_KEY already configured"
fi

if needs_generated_secret "ASTRA_JWT_SECRET"; then
    JWT_KEY="$(generate_secret -hex 32)"
    update_or_add "ASTRA_JWT_SECRET" "$JWT_KEY"
    echo "✅ Generated ASTRA_JWT_SECRET"
else
    echo "✅ ASTRA_JWT_SECRET already configured"
fi

if needs_generated_secret "ASTRA_RUNTIME_ROOT_SECRET"; then
    RUNTIME_ROOT_KEY="$(generate_secret -hex 32)"
    update_or_add "ASTRA_RUNTIME_ROOT_SECRET" "$RUNTIME_ROOT_KEY"
    echo "✅ Generated ASTRA_RUNTIME_ROOT_SECRET"
else
    echo "✅ ASTRA_RUNTIME_ROOT_SECRET already configured"
fi

if needs_generated_secret "MEMORIA_MASTER_KEY"; then
    MEMORIA_KEY="$(generate_secret -hex 32)"
    update_or_add "MEMORIA_MASTER_KEY" "$MEMORIA_KEY"
    echo "✅ Generated MEMORIA_MASTER_KEY"
else
    echo "✅ MEMORIA_MASTER_KEY already configured"
fi

# ── Optional: fast linker (mold) ──
CARGO_CONFIG="$PROJECT_ROOT/.cargo/config.toml"
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
echo "  1. Configure .env and copy .models.yaml.example to .models.yaml"
echo "  2. Build the CLI:    make build-cli-debug"
echo "  3. Start Server:     make dev-start"
echo "  4. Bootstrap:        ./target/debug/astra admin register"
