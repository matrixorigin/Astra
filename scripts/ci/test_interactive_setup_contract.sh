#!/usr/bin/env bash
# Static contract for the human-facing first-run path. It must remain safe to
# invoke accidentally in CI: this test never starts Docker or prompts.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo_root/scripts/setup/stack-setup.sh"
makefile="$repo_root/Makefile"
cli_setup="$repo_root/crates/astra-cli/src/admin_cli/setup.rs"
embedding_probe="$repo_root/scripts/setup/check_embedding.py"

grep -q '^stack-setup:' "$makefile"
grep -q '^stack-start: stack-env' "$makefile"
grep -q '@$(MAKE) stack-up' "$makefile"
grep -q '@$(MAKE) stack-verify' "$makefile"
grep -q 'Next: astra admin setup' "$makefile"
grep -q 'make --no-print-directory stack-env' "$script"
grep -q 'make --no-print-directory stack-up' "$script"
grep -q 'make --no-print-directory stack-verify' "$script"
grep -q 'admin setup' "$script"
grep -q '! -t 0 || ! -t 1' "$script"
grep -q 'chmod 600' "$script"
grep -q 'check_embedding.py' "$script"
grep -q 'STACK_RECREATE=1' "$script"
grep -q 'Repair containers and network' "$script"
grep -q 'Stop services and exit' "$script"
grep -q 'ensure_host_port ASTRA_API_PORT' "$script"
grep -q 'admin setup --help' "$script"
grep -q 'stop_detected_api' "$script"
grep -q 'guided setup cannot safely edit .env while exported overrides are active' "$script"
grep -q 'MEMORIA_EMBEDDING_ENDPOINTS is configured' "$script"
grep -q 'Edge: astra-edge --help' "$script"
if grep -q '\$cli edge --help' "$script"; then
    echo "interactive setup contract failed: astra edge is a chat message, not the User Runner command" >&2
    exit 1
fi
grep -q 'Password::new' "$cli_setup"
grep -q 'get_health_text' "$cli_setup"
grep -q 'model_check' "$cli_setup"
grep -q '/embeddings' "$embedding_probe"
grep -q 'dimension mismatch' "$embedding_probe"
grep -q 'non-numeric vector' "$embedding_probe"

fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/astra-interactive-setup.XXXXXX")"
trap 'rm -rf "$fixture_dir"' EXIT
fixture="$fixture_dir/stack.env"
generated_env="$fixture_dir/generated.env"
make --no-print-directory -C "$repo_root" stack-env STACK_ENV="$generated_env" >/dev/null
. "$repo_root/scripts/lib/env_file.sh"
for secret_name in ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET MEMORIA_MASTER_KEY; do
    if ! env_file_has_configured_value "$generated_env" "$secret_name"; then
        echo "interactive setup contract failed: stack-env did not generate $secret_name" >&2
        exit 1
    fi
done
generated_mode="$(stat -c '%a' "$generated_env" 2>/dev/null || stat -f '%Lp' "$generated_env")"
if [[ "$generated_mode" != 600 ]]; then
    echo "interactive setup contract failed: generated stack env mode is $generated_mode, expected 600" >&2
    exit 1
fi

# Existing fully configured files must also be repaired; secret generation is
# not guaranteed to rewrite their mode.
chmod 644 "$generated_env"
make --no-print-directory -C "$repo_root" stack-env STACK_ENV="$generated_env" >/dev/null
generated_mode="$(stat -c '%a' "$generated_env" 2>/dev/null || stat -f '%Lp' "$generated_env")"
if [[ "$generated_mode" != 600 ]]; then
    echo "interactive setup contract failed: existing stack env mode is $generated_mode, expected 600" >&2
    exit 1
fi

printf '%s\n' \
    'MEMORIA_EMBEDDING_PROVIDER=openai' \
    'MEMORIA_EMBEDDING_BASE_URL=https://api.openai.com/v1' \
    'MEMORIA_EMBEDDING_MODEL=text-embedding-3-small' \
    'MEMORIA_EMBEDDING_DIM=1536' \
    'MEMORIA_EMBEDDING_API_KEY=' > "$fixture"
if python3 "$embedding_probe" "$fixture" >/dev/null 2>&1; then
    echo "interactive setup contract failed: api.openai.com accepted an empty key" >&2
    exit 1
fi
printf '%s\n' 'MEMORIA_EMBEDDING_PROVIDER=mock' > "$fixture"
python3 "$embedding_probe" "$fixture" >/dev/null
python3 "$repo_root/scripts/ci/test_embedding_preflight.py"

if grep -Eq 'set -x|echo .*embedding_key|echo .*api_key' "$script"; then
    echo "interactive setup contract failed: secret may be traced or printed" >&2
    exit 1
fi

if grep -Eq '\$\{[^}]+,,\}|^[[:space:]]*select |dev-api-stop|recreate=true; break|start_stack true; break' "$script"; then
    echo "interactive setup contract failed: non-portable or unsafe recovery control flow" >&2
    exit 1
fi

echo "interactive setup contract: ok"
