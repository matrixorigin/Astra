#!/usr/bin/env bash
# Verify that the documented first-run setup produces safe, reusable local config.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-setup-contract.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

read_env_value() {
    local file="$1"
    local key="$2"
    awk -v key="$key" '
        /^[[:space:]]*#/ { next }
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            if (line ~ "^" key "[[:space:]]*=") {
                sub(/^[^=]*=/, "", line)
                sub(/^[[:space:]]*/, "", line)
                value = line
            }
        }
        END { print value }
    ' "$file"
}

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

snapshot_secrets() {
    local file="$1"
    local key
    for key in ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_JWT_SECRET ASTRA_RUNTIME_ROOT_SECRET MEMORIA_MASTER_KEY; do
        printf '%s=%s\n' "$key" "$(read_env_value "$file" "$key")"
    done
}

mkdir -p "$test_root/scripts/dev"
cp "$repo_root/.env.example" "$test_root/.env"
cp "$repo_root/scripts/dev/init.sh" "$test_root/scripts/dev/init.sh"
chmod +x "$test_root/scripts/dev/init.sh"

"$test_root/scripts/dev/init.sh" >/dev/null
first_snapshot="$(snapshot_secrets "$test_root/.env")"

while IFS='=' read -r key value; do
    if [[ -z "$value" || "$value" == *changeme* || "$value" == *change_me* || \
        "$value" == *change-me* || "$value" == your-* ]]; then
        echo "setup contract failed: $key was not generated safely" >&2
        exit 1
    fi
done <<< "$first_snapshot"

"$test_root/scripts/dev/init.sh" >/dev/null
second_snapshot="$(snapshot_secrets "$test_root/.env")"
if [[ "$first_snapshot" != "$second_snapshot" ]]; then
    echo "setup contract failed: init overwrote an existing secret" >&2
    exit 1
fi

set_env_value "$test_root/.env" "ASTRA_JWT_SECRET" "operator-supplied-secret"
"$test_root/scripts/dev/init.sh" >/dev/null
if [[ "$(read_env_value "$test_root/.env" "ASTRA_JWT_SECRET")" != "operator-supplied-secret" ]]; then
    echo "setup contract failed: init overwrote an operator-supplied secret" >&2
    exit 1
fi

stack_env="$test_root/stack.env"
cp "$repo_root/deployment/all-in-one/.env.example" "$stack_env"
for key in ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET MEMORIA_MASTER_KEY; do
    set_env_value "$stack_env" "$key" "test-${key}-value"
done
set_env_value "$stack_env" "MEMORIA_EMBEDDING_PROVIDER" "mock"
set_env_value "$stack_env" "MEMORIA_EMBEDDING_API_KEY" ""
set_env_value "$stack_env" "MEMORIA_EMBEDDING_BASE_URL" ""

make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null

set_env_value "$stack_env" "MEMORIA_EMBEDDING_PROVIDER" "openai"
if make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null 2>&1; then
    echo "setup contract failed: non-mock embeddings accepted empty credentials" >&2
    exit 1
fi

echo "setup contracts: ok"
