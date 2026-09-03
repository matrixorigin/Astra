#!/usr/bin/env bash
# Interactive, human-facing first-run setup for the all-in-one stack.
#
# Keep `make stack-start`/`make stack-up` suitable for automation. This script
# owns the local operator experience: embedding configuration, progress
# feedback, stack verification, and the API-level Astra admin wizard.

set -euo pipefail

if [[ ! -t 0 || ! -t 1 ]]; then
    echo "❌ make stack-setup needs an interactive terminal." >&2
    echo "   For CI or scripts, configure .env explicitly and run: make stack-up" >&2
    exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

stack_env="${STACK_ENV:-deployment/all-in-one/.env}"

die() {
    echo "❌ $*" >&2
    exit 1
}

ok() { printf '  \033[32m✓\033[0m %s\n' "$*"; }
step() { printf '\n\033[36m[%s]\033[0m %s\n' "$1" "$2"; }

set_env_value() {
    local key="$1" value="$2" temporary value_file
    temporary="$(mktemp "${TMPDIR:-/tmp}/astra-stack-env.XXXXXX")"
    value_file="$(mktemp "${TMPDIR:-/tmp}/astra-stack-value.XXXXXX")"
    chmod 600 "$value_file"
    printf '%s' "$value" > "$value_file"
    trap 'rm -f "$temporary" "$value_file"' RETURN
    ASTRA_SETUP_VALUE_FILE="$value_file" awk -v key="$key" '
        BEGIN {
            value_file = ENVIRON["ASTRA_SETUP_VALUE_FILE"]
            if ((getline file_value < value_file) > 0) value = file_value
            close(value_file)
        }
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
    ' "$stack_env" > "$temporary"
    chmod 600 "$temporary"
    mv "$temporary" "$stack_env"
    rm -f "$value_file"
    trap - RETURN
}

read_default() {
    local prompt="$1" default="$2" answer
    read -r -p "$prompt [$default]: " answer
    printf '%s' "${answer:-$default}"
}

resolve_cli() {
    if command -v astra >/dev/null 2>&1; then
        command -v astra
    elif [[ -x target/debug/astra ]]; then
        printf '%s' "$repo_root/target/debug/astra"
    elif [[ -x target/release/astra ]]; then
        printf '%s' "$repo_root/target/release/astra"
    else
        die "the astra CLI is not installed. Install it, or build it with 'make build-cli-debug', then rerun make stack-setup"
    fi
}

echo
printf '\033[1;36mAstra local setup\033[0m\n'
echo "This wizard configures local memory, starts the stack, and opens the admin setup."
echo "Secrets are hidden while typing and the local .env is restricted to your user."

cli="$(resolve_cli)"

step "1/5" "Preparing local secrets"
make --no-print-directory stack-env STACK_ENV="$stack_env"
chmod 600 "$stack_env"
ok "local stack configuration ready"

step "2/5" "Configuring semantic memory"
echo "Choose how Memoria creates embeddings:"
select embedding_choice in \
    "Mock embeddings — deterministic local evaluation (no API key)" \
    "OpenAI-compatible endpoint — recommended for real retrieval"; do
    case "$REPLY" in
        1|2) break ;;
        *) echo "  Please choose 1 or 2." ;;
    esac
done

if [[ "$REPLY" == 1 ]]; then
    set_env_value MEMORIA_EMBEDDING_PROVIDER mock
    set_env_value MEMORIA_EMBEDDING_BASE_URL ""
    set_env_value MEMORIA_EMBEDDING_API_KEY ""
    ok "mock embeddings selected (safe for evaluation; not production retrieval)"
else
    embedding_url="$(read_default 'Embedding base URL' 'https://api.openai.com/v1')"
    case "$embedding_url" in
        http://*|https://*) ;;
        *) die "embedding base URL must start with http:// or https://" ;;
    esac
    read -r -s -p "Embedding API key (leave blank if endpoint needs none): " embedding_key
    printf '\n'
    set_env_value MEMORIA_EMBEDDING_PROVIDER openai
    set_env_value MEMORIA_EMBEDDING_BASE_URL "$embedding_url"
    set_env_value MEMORIA_EMBEDDING_API_KEY "$embedding_key"
    unset embedding_key
    ok "embedding endpoint saved locally (key was not displayed)"
fi

step "3/5" "Starting Astra services"
make --no-print-directory stack-up STACK_ENV="$stack_env"
ok "MatrixOne, Memoria, and astra-server are running"

step "4/5" "Verifying the runtime"
make --no-print-directory stack-verify STACK_ENV="$stack_env"
ok "health check and memory round trip passed"

step "5/5" "Configuring administrator and model"
. scripts/lib/env_file.sh
api_port="$(env_resolve_value "$stack_env" ASTRA_API_PORT 2>/dev/null || true)"
bind_address="$(env_resolve_value "$stack_env" ASTRA_BIND_ADDRESS 2>/dev/null || true)"
api_host="$(env_http_host_from_bind "$bind_address")"
export ASTRA_API_URL="${ASTRA_API_URL:-http://${api_host}:${api_port:-17001}}"
"$cli" admin setup

echo
printf '\033[1;32mAstra is ready.\033[0m\n'
echo "  API:  $ASTRA_API_URL"
echo "  Chat: $cli chat -m \"Hello Astra\""
echo "  Edge: $cli edge --help (connect a local runner when you need private tools)"
