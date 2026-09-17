#!/usr/bin/env bash
# Static contract for the human-facing first-run path. It must remain safe to
# invoke accidentally in CI: this test never starts Docker or prompts.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo_root/scripts/setup/stack-setup.sh"
makefile="$repo_root/Makefile"
cli_setup="$repo_root/crates/astra-cli/src/admin_cli/setup.rs"
embedding_probe="$repo_root/scripts/setup/check_embedding.py"
identity_helpers="$repo_root/scripts/setup/stack_identity.sh"
status_helpers="$repo_root/scripts/setup/stack_status.sh"
env_write_helpers="$repo_root/scripts/setup/stack_env_write.sh"
memory_helpers="$repo_root/scripts/setup/stack_memory.sh"

grep -q '^stack-setup:' "$makefile"
grep -q '^stack-start: stack-env' "$makefile"
grep -q '@$(MAKE) stack-up' "$makefile"
grep -q '@$(MAKE) stack-verify' "$makefile"
grep -q 'Next: make stack-setup' "$makefile"
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
grep -q 'ensure_host_port ASTRA_API_PORT' "$identity_helpers"
grep -q 'admin setup --help' "$script"
grep -q 'stop_detected_api' "$script"
grep -q 'guided setup cannot safely edit .env while exported overrides are active' "$script"
grep -q 'MEMORIA_EMBEDDING_ENDPOINTS is configured' "$script"
grep -q 'Choose the intended outcome' "$script"
grep -q 'Create a separate installation' "$script"
grep -q 'compose --dry-run up' "$script"
grep -q -- '--project-name "$project_name"' "$script"
grep -q -- '--file "$stack_dir/docker-compose.yml"' "$script"
grep -q 'env -u COMPOSE_PROJECT_NAME -u COMPOSE_FILE' "$script"
grep -q -- '--project-name "$$project_name"' "$makefile"
grep -q -- '--file "$(abspath $(STACK_DIR)/docker-compose.yml)"' "$makefile"
grep -q 'Docker Compose is too old for safe change planning' "$script"
grep -q 'config set api_url' "$script"
grep -q 'Current installation status' "$script"
grep -q 'Finish the stack and configure chat later' "$script"
grep -q 'Chat configuration is present; provider connectivity was not rechecked' "$script"
grep -q 'Chat is not ready: configure an administrator and an active model' "$script"
grep -q 'admin_model_state' "$script"
grep -q 'print_stack_inspect_commands' "$script"
grep -q 'stack_env_write.sh' "$script"
grep -q 'trap cleanup_setup_temporary_files EXIT' "$script"
grep -q 'make stack-down STACK_ENV=' "$makefile"
grep -q 'TUI:.*cli_api_prefix' "$script"
grep -q 'Resume without restarting services' "$script"
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

# SIGTERM uses EXIT rather than a function RETURN trap. Both per-write files
# and an in-progress isolated descriptor must be removed on that path.
write_cleanup_dir="$fixture_dir/write-cleanup"
mkdir -p "$write_cleanup_dir"
set +e
TMPDIR="$write_cleanup_dir" bash -c '
    set -euo pipefail
    stack_env="$1/source.env"
    . "$2"
    stack_write_env_temp="$1/astra-stack-env.interrupted"
    stack_write_value_temp="$1/astra-stack-value.interrupted"
    stack_staging_env="$1/.env.isolated.env.staging.interrupted"
    printf "%s" complete-secret > "$stack_write_env_temp"
    printf "%s" api-key-secret > "$stack_write_value_temp"
    printf "%s" staged-secret > "$stack_staging_env"
    trap cleanup_setup_temporary_files EXIT
    trap "exit 130" TERM
    kill -TERM "$$"
' _ "$write_cleanup_dir" "$env_write_helpers"
cleanup_status=$?
set -e
[[ "$cleanup_status" == 130 ]]
if find "$write_cleanup_dir" -type f -print -quit | grep -q .; then
    echo "interactive setup contract failed: cancellation left secret-bearing temporary files" >&2
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
python3 "$repo_root/scripts/ci/test_user_memory_verification.py"

# Exercise self-hosted setup decisions without Docker or prompting.
(
    . "$env_write_helpers"
    . "$memory_helpers"
    stack_env="$fixture_dir/memory.env"
    supported_image="$(env_file_read "$repo_root/deployment/all-in-one/.env.example" MEMORIA_IMAGE)"
    [[ "$(env_file_read "$generated_env" MEMORIA_SELF_HOSTED_MASTER_ACCESS)" == 1 ]]
    [[ "$(env_file_read "$generated_env" MEMORIA_IMAGE)" == "$supported_image" ]]
    confirm() { [[ "$memory_answer" == yes ]]; }
    warn() { :; }
    ok() { :; }
    die() { exit 42; }
    printf 'MEMORIA_IMAGE=old-image\nMEMORIA_SELF_HOSTED_MASTER_ACCESS=0\n' > "$stack_env"
    memory_answer=no
    memory_changed=false
    configure_local_memory_access
    [[ "$memory_changed" == false ]]
    [[ "$(env_file_read "$stack_env" MEMORIA_SELF_HOSTED_MASTER_ACCESS)" == 0 ]]
    [[ "$(env_file_read "$stack_env" MEMORIA_IMAGE)" == old-image ]]
    memory_answer=yes
    configure_local_memory_access
    [[ "$memory_changed" == true ]]
    [[ "$(env_file_read "$stack_env" MEMORIA_SELF_HOSTED_MASTER_ACCESS)" == 1 ]]
    [[ "$(env_file_read "$stack_env" MEMORIA_IMAGE)" == "$supported_image" ]]
    memory_changed=false
    configure_local_memory_access
    [[ "$memory_changed" == false ]]
    set_env_value MEMORIA_IMAGE custom-image
    memory_answer=no
    if (configure_local_memory_access); then
        echo 'memory setup accepted an unverified custom image' >&2; exit 1
    else
        [[ "$?" == 42 ]]
    fi
    set_env_value MEMORIA_WEB_URL https://example.invalid
    set_env_value MEMORIA_SELF_HOSTED_MASTER_ACCESS 0
    memory_answer=yes
    configure_local_memory_access
    [[ "$(env_file_read "$stack_env" MEMORIA_SELF_HOSTED_MASTER_ACCESS)" == 0 ]]
    [[ "$(env_file_read "$stack_env" MEMORIA_IMAGE)" == custom-image ]]
)

if grep -Eq 'set -x|echo .*embedding_key|echo .*api_key' "$script"; then
    echo "interactive setup contract failed: secret may be traced or printed" >&2
    exit 1
fi

if grep -Eq '\$\{[^}]+,,\}|^[[:space:]]*select |dev-api-stop|recreate=true; break|start_stack true; break' "$script"; then
    echo "interactive setup contract failed: non-portable or unsafe recovery control flow" >&2
    exit 1
fi

# Exercise the installation-identity state changes without Docker. These are
# the destructive-boundary decisions behind the interactive wording, so a
# static grep is not enough.
identity_env="$fixture_dir/identity.env"
printf '%s\n' \
    'ASTRA_IMAGE=matrixorigin/astra:0.2.1' \
    'ASTRA_STACK_NAME=all-in-one' \
    'MATRIXONE_DATA_VOLUME=astra-matrixone-data' \
    'ASTRA_BIND_ADDRESS=127.0.0.1' \
    'ASTRA_API_PORT=17001' \
    'MEMORIA_PORT=8100' \
    'MATRIXONE_PORT=26001' \
    'MATRIXONE_DEBUG_HTTP_PORT=26060' > "$identity_env"
stack_env="$identity_env"
grep -q '^name: ${ASTRA_STACK_NAME:-all-in-one}' "$repo_root/deployment/all-in-one/docker-compose.yml"
grep -q 'ASTRA_STACK_ENV_FILE' "$repo_root/deployment/all-in-one/docker-compose.yml"
grep -q 'host.docker.internal:host-gateway' "$repo_root/deployment/all-in-one/docker-compose.yml"

set_env_value() {
    local key="$1" value="$2" temporary target
    target="${3:-${stack_env:-$identity_env}}"
    temporary="$target.tmp"
    awk -v key="$key" -v value="$value" '
        BEGIN { updated = 0 }
        $0 ~ "^" key "=" { print key "=" value; updated = 1; next }
        { print }
        END { if (!updated) print key "=" value }
    ' "$target" > "$temporary"
    mv "$temporary" "$target"
    identity_set_count=$((${identity_set_count:-0} + 1))
    if [[ -n "${identity_fail_after_set:-}" && "$identity_set_count" == "$identity_fail_after_set" ]]; then
        {
            printf '%s\n%s\n' "$stack_env" "$target"
            print_stack_command stack-down
            printf '\n'
        } > "$identity_interrupt_observation"
        return 1
    fi
}
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }
warn() { :; }
ok() { :; }
die() { exit 41; }
read_default() { prompt_value="$identity_answer"; }
choose() { menu_choice="$identity_choice"; }
read_tcp_port() {
    prompt_value="${identity_port_answers%% *}"
    if [[ "$identity_port_answers" == *' '* ]]; then
        identity_port_answers="${identity_port_answers#* }"
    else
        identity_port_answers=""
    fi
    identity_port_prompt_count=$((${identity_port_prompt_count:-0} + 1))
}
service_owns_host_port() { return 1; }
listener_pid() { printf '%s' "${identity_listener_pid:-}"; }
process_name() { printf '%s' "${identity_process_name:-}"; }
stop_detected_api() { return 1; }
cleanup_setup_temporary_files() {
    if [[ -n "${stack_staging_env:-}" ]]; then
        rm -f -- "$stack_staging_env"
        stack_staging_env=""
    fi
}
port_is_available() {
    [[ "$2" != "${identity_blocked_port:-}" ]] || return 1
    case " ${identity_blocked_ports:-} " in
        *" $2 "*) return 1 ;;
    esac
    return 0
}
docker() {
    local arg filter="" last_arg=""
    for arg in "$@"; do
        last_arg="$arg"
    done
    if [[ "${1:-}" == ps ]]; then
        for arg in "$@"; do
            case "$arg" in label=com.docker.compose.project=*) filter="${arg##*=}" ;; esac
        done
        if [[ -n "${identity_existing_project:-}" && "$filter" == "$identity_existing_project" ]]; then
            printf '%s\n' fake-container-id
        fi
    elif [[ "${1:-} ${2:-}" == "volume inspect" ]]; then
        if [[ " $* " == *' --format '* ]]; then
            printf '%s\n' "${identity_volume_owner:-}"
        elif [[ -n "${identity_existing_volume:-}" && "$last_arg" == "$identity_existing_volume" ]]; then
            return 0
        else
            return 1
        fi
    fi
}
. "$identity_helpers"
. "$status_helpers"
python_cmd=python3

ignored_descriptor="$(isolated_stack_env_path "$repo_root/deployment/all-in-one/.env" astra-0-2-1)"
[[ "$ignored_descriptor" == "$repo_root/deployment/all-in-one/.env.astra-0-2-1.env" ]]
git -C "$repo_root" check-ignore -q deployment/all-in-one/.env.astra-0-2-1.env
git -C "$repo_root" check-ignore -q deployment/all-in-one/.env.astra-0-2-1.env.staging.fixture

parse_model_catalog_state '{"items":[{"name":"inactive-only","is_active":false}]}'
[[ "$active_model_count" == 0 ]]
[[ -z "$active_model_names" ]]
[[ "$inactive_model_count" == 1 ]]
[[ "$inactive_model_names" == inactive-only ]]

compose_plan_keeps_service 'DRY-RUN MODE - Container astra-api-1 Running'
if compose_plan_keeps_service 'DRY-RUN MODE - Container astra-api-1 Recreate'; then
    echo "interactive setup contract failed: recreate plan was treated as reusable" >&2
    exit 1
fi
if compose_plan_keeps_service ''; then
    echo "interactive setup contract failed: empty plan was treated as reusable" >&2
    exit 1
fi

identity_existing_project=astra-0-2-1
identity_existing_volume=""
suggestion="$(suggest_isolated_stack_name)"
[[ "$suggestion" == astra-0-2-1-2 ]] || {
    echo "interactive setup contract failed: occupied installation name was not skipped" >&2
    exit 1
}

# A process-level project override must not redirect the management commands.
compose_preview="$(COMPOSE_PROJECT_NAME=all-in-one make --no-print-directory -n stack-status STACK_ENV="$identity_env")"
grep -q -- '--project-name "$project_name"' <<< "$compose_preview"
grep -q -- '--file "/.*deployment/all-in-one/docker-compose.yml"' <<< "$compose_preview"

# Port allocation is staged: a failure after the first allocation leaves the
# original descriptor byte-for-byte unchanged.
identity_blocked_ports=""

# Staging never changes the active descriptor. A failure after the first
# mutation cleans the ignored staging file and leaves recovery pointed at the
# original installation.
identity_answer=interrupted
identity_fail_after_set=1
identity_set_count=0
identity_interrupt_observation="$fixture_dir/interrupted.observation"
if (configure_isolated_stack >/dev/null 2>&1); then
    echo "interactive setup contract failed: injected staging interruption was accepted" >&2
    exit 1
fi
active_during_staging="$(sed -n '1p' "$identity_interrupt_observation")"
target_during_staging="$(sed -n '2p' "$identity_interrupt_observation")"
recovery_during_staging="$(sed -n '3p' "$identity_interrupt_observation")"
[[ "$active_during_staging" == "$identity_env" ]]
[[ "$recovery_during_staging" == "make stack-down STACK_ENV=$(printf '%q' "$identity_env")" ]]
case "$target_during_staging" in
    "$identity_env.interrupted.env.staging."*) ;;
    *) echo "interactive setup contract failed: staging descriptor is outside the ignored namespace" >&2; exit 1 ;;
esac
if find "$fixture_dir" -name 'identity.env.interrupted.env.staging.*' -print -quit | grep -q .; then
    echo "interactive setup contract failed: staging descriptor survived failure cleanup" >&2
    exit 1
fi
identity_fail_after_set=""
identity_set_count=0
for blocked_port in $(seq 8101 8200); do
    identity_blocked_ports="${identity_blocked_ports}${identity_blocked_ports:+ }${blocked_port}"
done
identity_answer=atomic-failure
before_identity_env="$(cksum < "$identity_env")"
if (configure_isolated_stack >/dev/null 2>&1); then
    echo "interactive setup contract failed: mid-allocation failure was accepted" >&2
    exit 1
fi
[[ "$(cksum < "$identity_env")" == "$before_identity_env" ]] || {
    echo "interactive setup contract failed: failed allocation changed the original env" >&2
    exit 1
}
identity_blocked_ports=""

identity_existing_project=""
identity_existing_volume=astra-0-2-1-matrixone-data
suggestion="$(suggest_isolated_stack_name)"
[[ "$suggestion" == astra-0-2-1-2 ]] || {
    echo "interactive setup contract failed: retained data volume name was not skipped" >&2
    exit 1
}

identity_existing_volume=""
identity_answer=journey-test
identity_blocked_port=17002
configure_isolated_stack >/dev/null
isolated_env="$stack_env"
[[ "$(env_file_read "$isolated_env" ASTRA_STACK_NAME)" == journey-test ]]
[[ "$(env_file_read "$isolated_env" MATRIXONE_DATA_VOLUME)" == journey-test-matrixone-data ]]
[[ "$(env_file_read "$isolated_env" MATRIXONE_LOG_DIR)" == ./data/stacks/journey-test/matrixone/logs ]]
[[ "$(env_file_read "$isolated_env" ASTRA_API_PORT)" == 17003 ]]
[[ "$(env_file_read "$isolated_env" MEMORIA_PORT)" == 8101 ]]
[[ "$(env_file_read "$identity_env" ASTRA_STACK_NAME)" == all-in-one ]]
for managed_env in "$identity_env" "$isolated_env"; do
    managed_preview="$(make --no-print-directory -n stack-status STACK_ENV="$managed_env")"
    managed_env_abs="$(cd "$(dirname "$managed_env")" && pwd)/$(basename "$managed_env")"
    grep -q -- "--env-file \"$managed_env_abs\"" <<< "$managed_preview"
    grep -q -- '--project-name "$project_name"' <<< "$managed_preview"
done
stack_env="$isolated_env"
isolated_env_quoted="$(printf '%q' "$isolated_env")"
[[ "$(print_stack_command stack-down)" == "make stack-down STACK_ENV=$isolated_env_quoted" ]]
failure_preview="$(make --no-print-directory -n stack-up STACK_ENV="$isolated_env")"
stack_down_recovery="$(grep -F 'make stack-down STACK_ENV=' <<< "$failure_preview")"
grep -Fq "$isolated_env" <<< "$stack_down_recovery"
stack_env="$identity_env"

# The allocator must reserve ports selected earlier in the same pass.
set_env_value ASTRA_API_PORT 20000
set_env_value MEMORIA_PORT 20001
set_env_value MATRIXONE_PORT 20002
set_env_value MATRIXONE_DEBUG_HTTP_PORT 20003
identity_answer=converging-test
identity_blocked_port=20001
configure_isolated_stack >/dev/null
converging_env="$stack_env"
converging_ports="$(env_file_read "$converging_env" ASTRA_API_PORT) $(env_file_read "$converging_env" MEMORIA_PORT) $(env_file_read "$converging_env" MATRIXONE_PORT) $(env_file_read "$converging_env" MATRIXONE_DEBUG_HTTP_PORT)"
[[ "$(printf '%s\n' $converging_ports | sort -u | wc -l | tr -d ' ')" == 4 ]] || {
    echo "interactive setup contract failed: selected ports were not unique" >&2
    exit 1
}
stack_env="$identity_env"

# Final preflight rejects duplicate free ports. Selecting the same reserved
# port once more must reprompt instead of allowing Compose to partially start.
set_env_value ASTRA_API_PORT 31000
set_env_value MEMORIA_PORT 31000
set_env_value MATRIXONE_PORT 31002
set_env_value MATRIXONE_DEBUG_HTTP_PORT 31003
identity_choice=1
identity_port_answers="31000 31001"
identity_port_prompt_count=0
check_host_ports >/dev/null
[[ "$(env_file_read "$identity_env" MEMORIA_PORT)" == 31001 ]]
[[ "$identity_port_prompt_count" == 2 ]]

# Missing optional keys use their effective defaults without a failing reread.
awk '$0 !~ /^ASTRA_API_PORT=/' "$identity_env" > "$identity_env.tmp"
mv "$identity_env.tmp" "$identity_env"
set_env_value MEMORIA_PORT 32001
set_env_value MATRIXONE_PORT 32002
set_env_value MATRIXONE_DEBUG_HTTP_PORT 32003
identity_port_answers=""
identity_port_prompt_count=0
check_host_ports >/dev/null
[[ "$effective_host_port" == 32003 ]]
[[ "$identity_port_prompt_count" == 0 ]]

# Numerically equivalent text is normalized before reservation, so leading
# zeroes cannot bypass duplicate detection.
set_env_value ASTRA_API_PORT 031000
set_env_value MEMORIA_PORT 31000
set_env_value MATRIXONE_PORT 33002
set_env_value MATRIXONE_DEBUG_HTTP_PORT 33003
identity_port_answers="31001"
identity_port_prompt_count=0
check_host_ports >/dev/null
[[ "$(env_file_read "$identity_env" MEMORIA_PORT)" == 31001 ]]
[[ "$identity_port_prompt_count" == 1 ]]

set_env_value ASTRA_STACK_NAME requested-name
set_env_value MATRIXONE_DATA_VOLUME shared-volume
identity_existing_volume=shared-volume
identity_volume_owner=older-installation
identity_choice=1
identity_answer=journey-recovered
identity_blocked_port=""
ensure_data_volume_is_not_shared >/dev/null
recovered_env="$stack_env"
[[ "$(env_file_read "$recovered_env" ASTRA_STACK_NAME)" == journey-recovered ]]
[[ "$(env_file_read "$recovered_env" MATRIXONE_DATA_VOLUME)" == journey-recovered-matrixone-data ]]
[[ "$(env_file_read "$identity_env" ASTRA_STACK_NAME)" == requested-name ]]
stack_env="$identity_env"
identity_existing_volume=""

# An existing volume without a trusted owner label is also unsafe; it must not
# be silently attached to the selected installation.
set_env_value ASTRA_STACK_NAME requested-name
set_env_value MATRIXONE_DATA_VOLUME unlabeled-volume
identity_existing_volume=unlabeled-volume
identity_volume_owner=""
identity_choice=1
identity_answer=unlabeled-recovered
ensure_data_volume_is_not_shared >/dev/null
unlabeled_env="$stack_env"
[[ "$(env_file_read "$unlabeled_env" MATRIXONE_DATA_VOLUME)" == unlabeled-recovered-matrixone-data ]]
stack_env="$identity_env"
identity_existing_volume=""

set_env_value ASTRA_STACK_NAME 'Invalid Name'
identity_volume_owner=""
if (ensure_data_volume_is_not_shared >/dev/null 2>&1); then
    echo "interactive setup contract failed: invalid installation name was accepted" >&2
    exit 1
else
    status=$?
    [[ "$status" == 41 ]] || exit "$status"
fi

echo "interactive setup contract: ok"
