#!/usr/bin/env bash
# Reject incomplete, placeholder, or obviously unsafe production configuration.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$script_dir/../lib/env_file.sh"

env_file="${1:-}"
if [[ -z "$env_file" || ! -f "$env_file" ]]; then
    echo "Error: production environment file not found: ${env_file:-<unset>}" >&2
    exit 1
fi

required_keys=(
    ASTRA_IMAGE
    MATRIXONE_HOST
    MATRIXONE_USER
    MATRIXONE_PASSWORD
    ASTRA_DATABASE
    ASTRA_CORS_ORIGINS
    ASTRA_JWT_SECRET
    ASTRA_TOKEN_ENCRYPTION_KEY
    ASTRA_RUNTIME_ROOT_SECRET
    MEMORIA_BASE_URL
    MEMORIA_MASTER_KEY
)

declare -a invalid_keys=()
for key in "${required_keys[@]}"; do
    value="$(env_file_read "$env_file" "$key" 2>/dev/null || true)"
    if env_value_is_placeholder "$value"; then
        invalid_keys+=("$key")
    fi
done

if ((${#invalid_keys[@]})); then
    echo "Error: missing or placeholder production configuration: ${invalid_keys[*]}" >&2
    exit 1
fi

astra_image="$(env_file_read "$env_file" ASTRA_IMAGE)"
image_name="${astra_image##*/}"
if [[ "$astra_image" == *:latest || ("$astra_image" != *@sha256:* && "$image_name" != *:*) ]]; then
    echo "Error: ASTRA_IMAGE must use an immutable tag or digest, not latest or an implicit tag." >&2
    exit 1
fi

if [[ "$(env_file_read "$env_file" ASTRA_CORS_ORIGINS)" == "*" ]]; then
    echo "Error: ASTRA_CORS_ORIGINS must list trusted production origins, not '*'." >&2
    exit 1
fi

for key in ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET; do
    value="$(env_file_read "$env_file" "$key")"
    if ((${#value} < 32)); then
        echo "Error: $key must contain at least 32 characters." >&2
        exit 1
    fi
done

memoria_master_key="$(env_file_read "$env_file" MEMORIA_MASTER_KEY)"
if ((${#memoria_master_key} < 16)); then
    echo "Error: MEMORIA_MASTER_KEY must contain at least 16 characters." >&2
    exit 1
fi

echo "production environment: ok"
