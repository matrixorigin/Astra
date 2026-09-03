#!/usr/bin/env bash
# Reject incomplete, placeholder, or obviously unsafe production configuration.

set -euo pipefail

env_file="${1:-}"
if [[ -z "$env_file" || ! -f "$env_file" ]]; then
    echo "Error: production environment file not found: ${env_file:-<unset>}" >&2
    exit 1
fi

read_env_value() {
    local key="$1"
    awk -v key="$key" '
        /^[[:space:]]*#/ { next }
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            if (line ~ "^" key "[[:space:]]*=") {
                found = 0
                value = ""
                sub(/^[^=]*=/, "", line)
                sub(/\r$/, "", line)
                sub(/^[[:space:]]*/, "", line)
                sub(/[[:space:]]*$/, "", line)
                first = substr(line, 1, 1)
                if (first == "\"" || first == "\047") {
                    closed = 0
                    for (i = 2; i <= length(line); i++) {
                        if (substr(line, i, 1) == first && substr(line, i - 1, 1) != "\\") {
                            value = substr(line, 2, i - 2)
                            closed = 1
                            break
                        }
                    }
                    if (!closed) next
                } else {
                    sub(/[[:space:]]+#.*/, "", line)
                    sub(/[[:space:]]*$/, "", line)
                    value = line
                }
                found = 1
            }
        }
        END {
            if (!found) exit 1
            print value
        }
    ' "$env_file"
}

is_placeholder() {
    local value
    value="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
    [[ -z "$value" || "$value" == *changeme* || "$value" == *change_me* || \
        "$value" == *change-me* || "$value" == *your-domain* || \
        "$value" == your-* || "$value" == *placeholder* ]]
}

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
    value="$(read_env_value "$key" 2>/dev/null || true)"
    if is_placeholder "$value"; then
        invalid_keys+=("$key")
    fi
done

if ((${#invalid_keys[@]})); then
    echo "Error: missing or placeholder production configuration: ${invalid_keys[*]}" >&2
    exit 1
fi

astra_image="$(read_env_value ASTRA_IMAGE)"
image_name="${astra_image##*/}"
if [[ "$astra_image" == *:latest || ("$astra_image" != *@sha256:* && "$image_name" != *:*) ]]; then
    echo "Error: ASTRA_IMAGE must use an immutable tag or digest, not latest or an implicit tag." >&2
    exit 1
fi

if [[ "$(read_env_value ASTRA_CORS_ORIGINS)" == "*" ]]; then
    echo "Error: ASTRA_CORS_ORIGINS must list trusted production origins, not '*'." >&2
    exit 1
fi

for key in ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET; do
    value="$(read_env_value "$key")"
    if ((${#value} < 32)); then
        echo "Error: $key must contain at least 32 characters." >&2
        exit 1
    fi
done

memoria_master_key="$(read_env_value MEMORIA_MASTER_KEY)"
if ((${#memoria_master_key} < 16)); then
    echo "Error: MEMORIA_MASTER_KEY must contain at least 16 characters." >&2
    exit 1
fi

echo "production environment: ok"
