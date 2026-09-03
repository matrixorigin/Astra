#!/usr/bin/env bash
# Verify that the documented first-run setup produces safe, reusable local config.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-setup-contract.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT

# Keep the contract deterministic when a developer's shell already exports
# stack configuration. Individual precedence cases set their own overrides.
for key in ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET \
    MEMORIA_MASTER_KEY MEMORIA_EMBEDDING_PROVIDER MEMORIA_EMBEDDING_API_KEY \
    MEMORIA_EMBEDDING_BASE_URL; do
    unset "$key"
done

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
mkdir -p "$test_root/scripts/lib"
cp "$repo_root/.env.example" "$test_root/.env"
cp "$repo_root/scripts/dev/init.sh" "$test_root/scripts/dev/init.sh"
cp "$repo_root/scripts/lib/env_file.sh" "$test_root/scripts/lib/env_file.sh"
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
set_env_value "$stack_env" "ASTRA_JWT_SECRET" '"operator-supplied-stack-secret" # retained'
set_env_value "$stack_env" "MEMORIA_EMBEDDING_PROVIDER" '"MoCk" # local evaluation'
set_env_value "$stack_env" "MEMORIA_EMBEDDING_API_KEY" ""
set_env_value "$stack_env" "MEMORIA_EMBEDDING_BASE_URL" ""

stack_secret_before="$(grep '^ASTRA_JWT_SECRET=' "$stack_env")"
make --no-print-directory -s -C "$repo_root" stack-env STACK_ENV="$stack_env" >/dev/null
if [[ "$(grep '^ASTRA_JWT_SECRET=' "$stack_env")" != "$stack_secret_before" ]]; then
    echo "setup contract failed: stack-env overwrote a quoted operator secret" >&2
    exit 1
fi

make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null

# The public configuration has one runtime root secret. Compose must project it
# to both the current input and the legacy input accepted by already-published
# Astra images, without requiring users to configure a second secret.
compose_file="$repo_root/deployment/all-in-one/docker-compose.yml"
if [[ "$(grep -c 'ASTRA_RUNTIME_ROOT_SECRET:.*ASTRA_RUNTIME_ROOT_SECRET' "$compose_file")" -lt 1 ]] ||
    [[ "$(grep -c 'ASTRA_BRIDGE_SECRET:.*ASTRA_RUNTIME_ROOT_SECRET' "$compose_file")" -ne 1 ]]; then
    echo "setup contract failed: compose does not project the canonical runtime root secret to both image generations" >&2
    exit 1
fi
if grep -q '^ASTRA_BRIDGE_SECRET=' "$repo_root/deployment/all-in-one/.env.example"; then
    echo "setup contract failed: legacy image input leaked into operator configuration" >&2
    exit 1
fi
if [[ "$(grep -c '^[[:space:]]*memoria-init:' "$compose_file")" -lt 2 ]] ||
    [[ "$(grep -c 'condition: service_completed_successfully' "$compose_file")" -lt 2 ]]; then
    echo "setup contract failed: Memoria log ownership is not initialized before startup" >&2
    exit 1
fi
if [[ "$(grep -c -- '--no-dereference' "$compose_file")" -ne 2 ]] ||
    [[ "$(grep -c 'must be a regular file' "$compose_file")" -ne 2 ]]; then
    echo "setup contract failed: privileged ownership initialization can follow unsafe persistent paths" >&2
    exit 1
fi
if [[ "$(grep -c 'HOST_UID:.*UID' "$compose_file")" -ne 2 ]] ||
    [[ "$(grep -c 'UID must be numeric' "$compose_file")" -ne 2 ]]; then
    echo "setup contract failed: privileged ownership initialization accepts an unvalidated UID" >&2
    exit 1
fi

set_env_value "$stack_env" "MEMORIA_EMBEDDING_PROVIDER" "openai"
set_env_value "$stack_env" "MEMORIA_EMBEDDING_API_KEY" " # still empty"
set_env_value "$stack_env" "MEMORIA_EMBEDDING_BASE_URL" " # still empty"
if make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null 2>&1; then
    echo "setup contract failed: non-mock embeddings accepted a missing endpoint" >&2
    exit 1
fi

MEMORIA_EMBEDDING_PROVIDER=mock \
    make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null

if ASTRA_JWT_SECRET= MEMORIA_EMBEDDING_PROVIDER=mock \
    make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null 2>&1; then
    echo "setup contract failed: an explicitly empty shell override did not override the env file" >&2
    exit 1
fi

MEMORIA_EMBEDDING_BASE_URL=http://embeddings.internal/v1 \
    make --no-print-directory -s -C "$repo_root" stack-check-env STACK_ENV="$stack_env" >/dev/null

# Stack status, verification, and edge startup must all derive a connectable
# local URL from the same bind address Compose uses.
# shellcheck source=../lib/env_file.sh
. "$repo_root/scripts/lib/env_file.sh"
[[ "$(env_http_host_from_bind 0.0.0.0)" == "127.0.0.1" ]]
[[ "$(env_http_host_from_bind 192.0.2.10)" == "192.0.2.10" ]]
[[ "$(env_http_host_from_bind ::)" == "[::1]" ]]
[[ "$(env_http_host_from_bind 2001:db8::10)" == "[2001:db8::10]" ]]

echo "setup contracts: ok"
