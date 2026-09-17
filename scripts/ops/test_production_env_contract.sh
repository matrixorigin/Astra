#!/usr/bin/env bash
# Exercise production environment validation without Docker or external services.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-production-env-contract.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

set_env_value() {
    local file="$1"
    local key="$2"
    local value="$3"
    local temporary
    temporary="$(mktemp "${test_root}/env.XXXXXX")"
    awk -v key="$key" -v value="$value" '
        BEGIN { updated = 0 }
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            if (line ~ "^" key "[[:space:]]*=") {
                print key "=" value
                updated = 1
                next
            }
            print
        }
        END { if (!updated) print key "=" value }
    ' "$file" > "$temporary"
    mv "$temporary" "$file"
}

expect_rejected() {
    if "$repo_root/scripts/ops/validate_production_env.sh" "$1" >/dev/null 2>&1; then
        echo "production environment contract failed: invalid configuration was accepted" >&2
        exit 1
    fi
}

production_env="$test_root/.env.production"
cp "$repo_root/.env.production.example" "$production_env"
expect_rejected "$production_env"

if deploy_error="$(ASTRA_PRODUCTION_ENV_FILE="$production_env" "$repo_root/scripts/ops/deploy.sh" 2>&1)"; then
    echo "production environment contract failed: deploy accepted template configuration" >&2
    exit 1
fi
if [[ "$deploy_error" != *"missing or placeholder production configuration"* ]]; then
    echo "production environment contract failed: deploy did not run the environment gate first" >&2
    exit 1
fi

set_env_value "$production_env" MATRIXONE_HOST db.internal
set_env_value "$production_env" MATRIXONE_USER astra
set_env_value "$production_env" MATRIXONE_PASSWORD a-production-database-password
set_env_value "$production_env" ASTRA_DATABASE astra_runtime
set_env_value "$production_env" ASTRA_CORS_ORIGINS https://astra.example.com
set_env_value "$production_env" ASTRA_JWT_SECRET 0123456789abcdef0123456789abcdef
set_env_value "$production_env" ASTRA_TOKEN_ENCRYPTION_KEY 123456789abcdef0123456789abcdef0
set_env_value "$production_env" ASTRA_RUNTIME_ROOT_SECRET 23456789abcdef0123456789abcdef01
set_env_value "$production_env" MEMORIA_BASE_URL https://memoria.internal
set_env_value "$production_env" MEMORIA_MASTER_KEY 3456789abcdef0123456789abcdef012

"$repo_root/scripts/ops/validate_production_env.sh" "$production_env" >/dev/null

production_compose="$repo_root/deployment/all-in-one/docker-compose.prod.yml"
if [[ "$(grep -c 'ASTRA_BRIDGE_SECRET:.*ASTRA_RUNTIME_ROOT_SECRET' "$production_compose")" -ne 1 ]]; then
    echo "production environment contract failed: immutable pre-rename images cannot consume the canonical runtime root secret" >&2
    exit 1
fi

set_env_value "$production_env" ASTRA_IMAGE matrixorigin/astra:latest
expect_rejected "$production_env"
set_env_value "$production_env" ASTRA_IMAGE matrixorigin/astra:0.1.0

set_env_value "$production_env" ASTRA_CORS_ORIGINS '*'
expect_rejected "$production_env"
set_env_value "$production_env" ASTRA_CORS_ORIGINS https://astra.example.com

set_env_value "$production_env" ASTRA_TOKEN_ENCRYPTION_KEY too-short
expect_rejected "$production_env"

set_env_value "$production_env" ASTRA_TOKEN_ENCRYPTION_KEY 'too-short # a comment cannot pad a secret'
expect_rejected "$production_env"

echo "production environment contracts: ok"
