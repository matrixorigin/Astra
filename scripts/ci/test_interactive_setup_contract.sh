#!/usr/bin/env bash
# Static contract for the human-facing first-run path. It must remain safe to
# invoke accidentally in CI: this test never starts Docker or prompts.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo_root/scripts/setup/stack-setup.sh"
makefile="$repo_root/Makefile"
cli_setup="$repo_root/crates/astra-cli/src/admin_cli/setup.rs"

grep -q '^stack-setup:' "$makefile"
grep -q '^stack-start: stack-up' "$makefile"
grep -q 'make --no-print-directory stack-env' "$script"
grep -q 'make --no-print-directory stack-up' "$script"
grep -q 'make --no-print-directory stack-verify' "$script"
grep -q 'admin setup' "$script"
grep -q '! -t 0 || ! -t 1' "$script"
grep -q 'chmod 600' "$script"
grep -q 'Password::new' "$cli_setup"
grep -q 'get_health_text' "$cli_setup"
grep -q 'model_check' "$cli_setup"

if grep -Eq 'set -x|echo .*embedding_key|echo .*api_key' "$script"; then
    echo "interactive setup contract failed: secret may be traced or printed" >&2
    exit 1
fi

echo "interactive setup contract: ok"
