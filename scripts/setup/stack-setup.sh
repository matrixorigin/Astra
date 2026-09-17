#!/usr/bin/env bash
# State-aware, interactive first-run setup for the all-in-one stack.

set -euo pipefail

if [[ ! -t 0 || ! -t 1 ]]; then
    echo "❌ make stack-setup needs an interactive terminal." >&2
    echo "   For CI or scripts, configure .env explicitly and run: make stack-up" >&2
    exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

stack_env="${STACK_ENV:-deployment/all-in-one/.env}"
if [[ "$stack_env" != /* ]]; then
    stack_env="$repo_root/$stack_env"
fi
stack_dir="$repo_root/deployment/all-in-one"
. "$repo_root/scripts/setup/stack_status.sh"
. "$repo_root/scripts/setup/stack_env_write.sh"
. "$repo_root/scripts/setup/stack_memory.sh"

die() {
    echo "❌ $*" >&2
    exit 1
}

ok() { printf '  \033[32m✓\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }
step() { printf '\n\033[1;36m[%s]\033[0m %s\n' "$1" "$2"; }
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

cancel_setup() {
    printf '\nSetup canceled. No persistent data was deleted.\n'
    exit 0
}

on_interrupt() {
    printf '\n\nSetup interrupted. No persistent data was deleted.\n' >&2
    printf 'Services may be partially started if Compose was already running.\n' >&2
    print_stack_inspect_commands >&2
    printf 'Stop safely: ' >&2
    print_stack_command stack-down >&2
    printf '\n' >&2
    exit 130
}
trap on_interrupt INT TERM
trap cleanup_setup_temporary_files EXIT

command -v docker >/dev/null 2>&1 || die "docker is required"
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required"
docker compose up --help 2>/dev/null | grep -q -- '--dry-run' ||
    die "Docker Compose is too old for safe change planning; upgrade to a version whose 'docker compose up --help' includes --dry-run"
docker info >/dev/null 2>&1 || die "Docker is not running or is not accessible"

python_cmd=""
for candidate in python3 python; do
    if command -v "$candidate" >/dev/null 2>&1 &&
        "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 9))' >/dev/null 2>&1; then
        python_cmd="$candidate"
        break
    fi
done
[[ -n "$python_cmd" ]] || die "Python 3.9 or newer is required"

# Compose gives exported shell variables precedence over the selected env file.
# The wizard edits that file, so allowing an invisible higher-precedence value
# would make its prompts, preflight, and actual containers disagree.
setup_overrides=""
for key in \
    MEMORIA_EMBEDDING_PROVIDER MEMORIA_EMBEDDING_BASE_URL \
    MEMORIA_EMBEDDING_MODEL MEMORIA_EMBEDDING_DIM MEMORIA_EMBEDDING_API_KEY \
    MEMORIA_EMBEDDING_ENDPOINTS ASTRA_STACK_NAME MATRIXONE_DATA_VOLUME \
    MATRIXONE_LOG_DIR MEMORIA_LOG_DIR ASTRA_BIND_ADDRESS ASTRA_API_URL \
    ASTRA_API_PORT MEMORIA_PORT MATRIXONE_PORT MATRIXONE_DEBUG_HTTP_PORT \
    MEMORIA_IMAGE MEMORIA_SELF_HOSTED_MASTER_ACCESS MEMORIA_WEB_URL; do
    if printenv "$key" >/dev/null 2>&1; then
        setup_overrides="${setup_overrides}${setup_overrides:+, }$key"
    fi
done
if [[ -n "$setup_overrides" ]]; then
    die "guided setup cannot safely edit .env while exported overrides are active: $setup_overrides
   Unset them and rerun make stack-setup, or use make stack-start for environment-driven automation."
fi

compose() {
    local project_name
    project_name="$(env_file_read "$stack_env" ASTRA_STACK_NAME 2>/dev/null || true)"
    project_name="${project_name:-all-in-one}"
    (
        cd "$stack_dir"
        env -u COMPOSE_PROJECT_NAME -u COMPOSE_FILE \
            UID="$(id -u)" GID="$(id -g)" ASTRA_STACK_ENV_FILE="$stack_env" \
            docker compose --project-name "$project_name" \
            --file "$stack_dir/docker-compose.yml" --env-file "$stack_env" "$@"
    )
}

read_default() {
    local prompt="$1" default="$2" answer
    read -r -p "$prompt [$default]: " answer || cancel_setup
    prompt_value="${answer:-$default}"
}

confirm() {
    local prompt="$1" default="${2:-yes}" answer normalized suffix
    if [[ "$default" == yes ]]; then suffix="Y/n"; else suffix="y/N"; fi
    while true; do
        read -r -p "$prompt [$suffix]: " answer || cancel_setup
        answer="${answer:-$default}"
        normalized="$(printf '%s' "$answer" | tr '[:upper:]' '[:lower:]')"
        case "$normalized" in
            y|yes) return 0 ;;
            n|no) return 1 ;;
            *) echo "  Enter yes or no." ;;
        esac
    done
}

choose() {
    local prompt="$1" answer index
    shift
    echo "$prompt"
    index=1
    for option in "$@"; do
        printf '  %d) %s\n' "$index" "$option"
        index=$((index + 1))
    done
    while true; do
        read -r -p "Select [1-$#]: " answer || cancel_setup
        case "$answer" in
            ''|*[!0-9]*) echo "  Enter a number from 1 to $#." ;;
            *)
                answer=$((10#$answer))
                if ((answer >= 1 && answer <= $#)); then
                    menu_choice="$answer"
                    return 0
                fi
                echo "  Enter a number from 1 to $#."
                ;;
        esac
    done
}

read_positive_integer() {
    local prompt="$1" default="$2" answer
    while true; do
        read_default "$prompt" "$default"
        answer="$prompt_value"
        case "$answer" in
            ''|*[!0-9]*|0) echo "  Enter a positive whole number." >&2 ;;
            *) prompt_value="$answer"; return 0 ;;
        esac
    done
}

read_tcp_port() {
    local prompt="$1" default="$2" answer
    while true; do
        read_positive_integer "$prompt" "$default"
        answer="$prompt_value"
        answer=$((10#$answer))
        if ((answer <= 65535)); then
            prompt_value="$answer"
            return 0
        fi
        echo "  Enter a TCP port from 1 to 65535." >&2
    done
}

resolve_cli() {
    local candidate installed=""
    if command -v astra >/dev/null 2>&1; then
        installed="$(command -v astra)"
        if "$installed" admin setup --help >/dev/null 2>&1; then
            printf '%s' "$installed"
            return 0
        fi
    fi
    for candidate in "$repo_root/target/debug/astra" "$repo_root/target/release/astra"; do
        if [[ -x "$candidate" ]] && "$candidate" admin setup --help >/dev/null 2>&1; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    if [[ -n "$installed" ]]; then
        die "installed astra CLI does not support 'admin setup'. Upgrade it or run 'make build-cli-debug'"
    fi
    die "the astra CLI is not installed. Install it, or build it with 'make build-cli-debug', then rerun make stack-setup"
}

refresh_embedding_state() {
    embedding_provider="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_PROVIDER 2>/dev/null || true)"
    embedding_provider="${embedding_provider:-openai}"
    embedding_url="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_BASE_URL 2>/dev/null || true)"
    embedding_model="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_MODEL 2>/dev/null || true)"
    embedding_dimension="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_DIM 2>/dev/null || true)"
    embedding_key="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_API_KEY 2>/dev/null || true)"
}

show_embedding_state() {
    echo "Current embedding configuration:"
    echo "  Provider:  $embedding_provider"
    if [[ "$(lower "$embedding_provider")" != mock ]]; then
        echo "  Endpoint:  ${embedding_url:-not configured}"
        echo "  Model:     ${embedding_model:-not configured}"
        echo "  Dimension: ${embedding_dimension:-not configured}"
        if [[ -n "$embedding_key" ]]; then
            echo "  API key:   configured (hidden)"
        else
            echo "  API key:   not configured"
        fi
    fi
}

configure_real_embedding() {
    local default_url default_model default_dimension new_key keep_key
    default_url="${embedding_url:-https://api.openai.com/v1}"
    read_default 'Embedding base URL' "$default_url"
    embedding_url="$prompt_value"
    case "$embedding_url" in
        http://*|https://*) ;;
        *) warn "embedding base URL must start with http:// or https://"; return 1 ;;
    esac

    if [[ "$embedding_url" == https://api.openai.com/v1* ]]; then
        if [[ "$embedding_model" == text-embedding-* ]]; then
            default_model="$embedding_model"
            default_dimension="${embedding_dimension:-1536}"
        else
            default_model="text-embedding-3-small"
            default_dimension="1536"
        fi
    else
        default_model="${embedding_model:-BAAI/bge-m3}"
        default_dimension="${embedding_dimension:-1024}"
    fi
    read_default 'Embedding model' "$default_model"
    embedding_model="$prompt_value"
    read_positive_integer 'Embedding dimension' "$default_dimension"
    embedding_dimension="$prompt_value"

    keep_key=false
    if [[ -n "$embedding_key" ]] && confirm "Keep the existing hidden embedding API key?" yes; then
        keep_key=true
    else
        read -r -s -p "Embedding API key (leave blank only for an unauthenticated endpoint): " new_key || cancel_setup
        printf '\n'
        embedding_key="$new_key"
        unset new_key
    fi
    if [[ "$embedding_url" == https://api.openai.com/v1* && -z "$embedding_key" ]]; then
        warn "api.openai.com requires an API key"
        return 1
    fi

    set_env_value MEMORIA_EMBEDDING_PROVIDER openai
    set_env_value MEMORIA_EMBEDDING_BASE_URL "$embedding_url"
    set_env_value MEMORIA_EMBEDDING_MODEL "$embedding_model"
    set_env_value MEMORIA_EMBEDDING_DIM "$embedding_dimension"
    if [[ "$keep_key" != true ]]; then
        set_env_value MEMORIA_EMBEDDING_API_KEY "$embedding_key"
    fi
    embedding_provider=openai
    return 0
}

configure_embedding() {
    while true; do
        choose "Choose how Memoria creates embeddings:" \
            "Mock embeddings — deterministic local evaluation (no API key)" \
            "OpenAI-compatible endpoint — real semantic retrieval"
        if [[ "$menu_choice" == 1 ]]; then
            set_env_value MEMORIA_EMBEDDING_PROVIDER mock
            set_env_value MEMORIA_EMBEDDING_BASE_URL ""
            set_env_value MEMORIA_EMBEDDING_API_KEY ""
            embedding_provider=mock
            embedding_url=""
            embedding_key=""
            ok "mock embeddings selected (evaluation only)"
            return 0
        fi
        if configure_real_embedding; then
            ok "embedding configuration saved locally; the API key was not displayed"
            return 0
        fi
        echo "  Please correct the embedding settings."
    done
}

probe_embedding() {
    while true; do
        if "$python_cmd" scripts/setup/check_embedding.py "$stack_env"; then
            if [[ "$(lower "$embedding_provider")" == mock ]]; then
                ok "mock embedding mode is ready"
            else
                ok "embedding endpoint, credentials, model, and dimension are valid"
            fi
            return 0
        fi
        warn "startup was not attempted because embedding preflight failed; existing services were left unchanged"
        choose "How would you like to continue?" \
            "Edit embedding settings and retry" \
            "Switch to mock embeddings" \
            "Exit and leave the existing stack unchanged"
        case "$menu_choice" in
                1)
                    configure_embedding
                    embedding_changed=true
                    refresh_embedding_state
                    continue
                    ;;
                2)
                    set_env_value MEMORIA_EMBEDDING_PROVIDER mock
                    set_env_value MEMORIA_EMBEDDING_BASE_URL ""
                    set_env_value MEMORIA_EMBEDDING_API_KEY ""
                    embedding_changed=true
                    refresh_embedding_state
                    continue
                    ;;
                3) echo "Startup was not attempted; the current configuration and service state were retained."; exit 0 ;;
        esac
    done
}

service_state() {
    local service="$1" container_id state networks
    container_id="$(compose ps -a -q "$service" 2>/dev/null | head -n 1)"
    if [[ -z "$container_id" ]]; then
        printf 'missing'
        return
    fi
    state="$(docker inspect --format '{{if .State.Running}}{{if .State.Health}}{{.State.Health.Status}}{{else}}running{{end}}{{else}}{{.State.Status}}{{end}}' "$container_id" 2>/dev/null || echo unknown)"
    networks="$(docker inspect --format '{{len .NetworkSettings.Networks}}' "$container_id" 2>/dev/null || echo 0)"
    if [[ "$networks" == 0 ]]; then
        printf '%s, network disconnected' "$state"
    else
        printf '%s' "$state"
    fi
}

stack_is_healthy() {
    [[ "$(service_state matrixone)" == healthy ]] &&
        [[ "$(service_state memoria)" == healthy ]] &&
        [[ "$(service_state api)" == healthy ]]
}

stack_exists() {
    [[ -n "$(compose ps -a -q 2>/dev/null)" ]]
}

service_matches_configuration() {
    local service="$1" plan
    # Compose's `config --hash` does not match the runtime label for every
    # env_file configuration. Ask Compose for its own read-only execution plan
    # so the wizard makes the same recreate decision as `compose up`.
    if ! plan="$(COMPOSE_ANSI=never compose --dry-run up -d --no-deps --no-build --pull never "$service" 2>&1)"; then
        return 1
    fi
    compose_plan_keeps_service "$plan"
}

stack_matches_configuration() {
    service_matches_configuration matrixone &&
        service_matches_configuration memoria &&
        service_matches_configuration api
}

service_owns_host_port() {
    local service="$1" host_port="$2" container_port="$3" container_id binding
    container_id="$(compose ps -a -q "$service" 2>/dev/null | head -n 1)"
    [[ -n "$container_id" ]] || return 1
    # `docker port` reports an effective binding for running and restarting
    # containers. A stopped container returns no binding and does not own the
    # host port, so no separate (and racy) Running-state check is needed.
    while IFS= read -r binding; do
        case "$binding" in
            *:"$host_port") return 0 ;;
        esac
    done < <(docker port "$container_id" "$container_port/tcp" 2>/dev/null || true)
    return 1
}

port_is_available() {
    local bind_address="$1" port="$2"
    "$python_cmd" - "$bind_address" "$port" <<'PY'
import socket
import sys

host = sys.argv[1].strip("[]") or "127.0.0.1"
port = int(sys.argv[2])
family = socket.AF_INET6 if ":" in host else socket.AF_INET
sock = socket.socket(family, socket.SOCK_STREAM)
try:
    sock.bind((host, port))
except OSError:
    raise SystemExit(1)
finally:
    sock.close()
PY
}

. "$repo_root/scripts/setup/stack_identity.sh"

listener_pid() {
    local port="$1" pid=""
    if command -v lsof >/dev/null 2>&1; then
        pid="$(lsof -nP -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null | head -n 1 || true)"
    elif command -v ss >/dev/null 2>&1; then
        pid="$(ss -ltnp "sport = :$port" 2>/dev/null | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p' | head -n 1 || true)"
    fi
    printf '%s' "$pid"
}

process_name() {
    local pid="$1"
    if [[ -r "/proc/$pid/comm" ]]; then
        sed -n '1p' "/proc/$pid/comm"
    else
        ps -p "$pid" -o comm= 2>/dev/null | sed -n '1p'
    fi
}

stop_detected_api() {
    local pid="$1" port="$2" attempt=0
    if [[ -z "$pid" ]] || [[ "$(listener_pid "$port")" != "$pid" ]] ||
        [[ "$(process_name "$pid")" != astra-server ]]; then
        warn "the detected process changed; the port will be checked again"
        return 1
    fi
    if ! kill "$pid" 2>/dev/null; then
        warn "could not stop astra-server PID $pid; stop it yourself or choose another port"
        return 1
    fi
    while kill -0 "$pid" 2>/dev/null && ((attempt < 10)); do
        sleep 1
        attempt=$((attempt + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        warn "astra-server PID $pid did not stop after 10 seconds"
        return 1
    fi
    ok "stopped the detected source-mode Astra API (PID $pid)"
}

stop_and_exit() {
    make --no-print-directory stack-down STACK_ENV="$stack_env"
    echo "Stack stopped. Persistent data was kept."
    exit 0
}

choose_existing_stack_action() {
    startup_mode=normal
    reuse_stack=false
    if ! stack_exists; then
        return
    fi

    echo "Existing local Astra installation detected:"
    compose ps -a
    echo
    echo "  matrixone: $(service_state matrixone)"
    echo "  memoria:   $(service_state memoria)"
    echo "  api:       $(service_state api)"

    if ! stack_matches_configuration; then
        warn "the existing installation differs from this release or the current .env"
        choose "Choose the intended outcome:" \
            "Update the existing installation (recreate containers, preserve its data)" \
            "Create a separate installation (new data and ports; existing installation untouched)" \
            "Leave the existing installation unchanged and exit"
        case "$menu_choice" in
            1) startup_mode=repair; return ;;
            2) configure_isolated_stack; startup_mode=normal; return ;;
            3) echo "Existing installation left unchanged."; exit 0 ;;
        esac
    fi
    if stack_is_healthy; then
        if confirm "Reuse this healthy stack without restarting it?" yes; then
            reuse_stack=true
            return
        fi
        choose "Choose what to do with the healthy stack:" \
            "Restart containers and preserve data" \
            "Stop services and exit" \
            "Leave services unchanged and exit"
        case "$menu_choice" in
                1) startup_mode=repair; return ;;
                2) stop_and_exit ;;
                3) echo "Existing stack left unchanged."; exit 0 ;;
        esac
    fi

    warn "the existing stack is partial, unhealthy, or disconnected"
    choose "Choose how to handle the unhealthy stack:" \
        "Repair containers and network (preserve data)" \
        "Stop services and exit (preserve data)" \
        "Leave the current state for inspection and exit"
    case "$menu_choice" in
            1) startup_mode=repair; return ;;
            2) stop_and_exit ;;
            3) echo "Existing stack left unchanged."; exit 0 ;;
    esac
}

start_stack() {
    local recreate="$1"
    while true; do
        check_host_ports
        echo "  Startup may take up to 3 minutes. Ctrl+C stops waiting but keeps data."
        if [[ "$recreate" == true ]]; then
            echo "  Recreating containers and network attachments; persistent volumes are preserved."
            if make --no-print-directory stack-up STACK_ENV="$stack_env" STACK_RECREATE=1; then
                return 0
            fi
        elif make --no-print-directory stack-up STACK_ENV="$stack_env"; then
            return 0
        fi

        warn "stack startup failed"
        choose "Choose a recovery action:" \
            "Repair and retry (recreate containers, preserve data)" \
            "Stop services and exit (preserve data)" \
            "Leave the current service state for inspection and exit"
        case "$menu_choice" in
                1) recreate=true; continue ;;
                2) stop_and_exit ;;
                3) print_stack_inspect_commands; exit 0 ;;
        esac
    done
}

verify_stack() {
    while true; do
        if make --no-print-directory stack-verify STACK_ENV="$stack_env"; then
            return 0
        fi
        warn "runtime verification failed; setup will not continue to admin/model configuration"
        choose "Choose a verification recovery action:" \
            "Retry verification" \
            "Repair stack and retry verification" \
            "Stop services and exit (preserve data)" \
            "Leave the current service state for inspection and exit"
        case "$menu_choice" in
                1) continue ;;
                2) start_stack true; continue ;;
                3) stop_and_exit ;;
                4) print_stack_inspect_commands; exit 0 ;;
        esac
    done
}

# Inspect server-side setup without printing credentials. This is deliberately
# a read-only status probe: setup can be rerun for a healthy stack and will
# explain exactly which user-layer pieces are still missing.
admin_model_state() {
    local whoami_json model_json
    admin_state="not configured"
    admin_identity=""
    active_model_count=0
    inactive_model_count=0
    active_model_names=""
    inactive_model_names=""

    if ASTRA_API_URL="$ASTRA_API_URL" "$cli" admin config list >/dev/null 2>&1; then
        admin_state="ready"
        whoami_json="$(ASTRA_API_URL="$ASTRA_API_URL" "$cli" admin whoami 2>/dev/null || true)"
        admin_identity="$("$python_cmd" -c '
import json
import sys

try:
    value = json.loads(sys.argv[1])
except (IndexError, json.JSONDecodeError):
    raise SystemExit(0)
if isinstance(value, dict):
    identity = value.get("username") or value.get("email") or value.get("user_id")
    if isinstance(identity, str) and identity.strip():
        print(identity.strip())
' "$whoami_json" 2>/dev/null || true)"
    else
        # A non-admin account may still be useful for a later login, but it
        # cannot complete this wizard's server-wide model configuration.
        whoami_json="$(ASTRA_API_URL="$ASTRA_API_URL" "$cli" admin whoami 2>/dev/null || true)"
        admin_identity="$("$python_cmd" -c '
import json
import sys

try:
    value = json.loads(sys.argv[1])
except (IndexError, json.JSONDecodeError):
    raise SystemExit(0)
if isinstance(value, dict):
    identity = value.get("username") or value.get("email") or value.get("user_id")
    if isinstance(identity, str) and identity.strip():
        print(identity.strip())
' "$whoami_json" 2>/dev/null || true)"
        if [[ -n "$admin_identity" ]]; then
            admin_state="signed in (admin role not verified)"
        fi
        return 0
    fi

    model_json="$(ASTRA_API_URL="$ASTRA_API_URL" "$cli" admin model list 2>/dev/null || true)"
    [[ -n "$model_json" ]] || return 0
    parse_model_catalog_state "$model_json"
}

show_setup_state() {
    local chat_state
    admin_model_state
    echo
    echo "Current installation status"
    echo "  Infrastructure: ready (API $ASTRA_API_URL; dependency memory round trip verified)"
    if [[ "$admin_state" == ready ]]; then
        if [[ -n "$admin_identity" ]]; then
            echo "  Administrator:  ready ($admin_identity)"
        else
            echo "  Administrator:  ready (authenticated admin profile)"
        fi
    else
        echo "  Administrator:  $admin_state"
    fi
    if ((active_model_count > 0)); then
        echo "  Models:          $active_model_count active${active_model_names:+ ($active_model_names)}"
    else
        echo "  Models:          no active model"
    fi
    if ((inactive_model_count > 0)); then
        echo "                   $inactive_model_count inactive${inactive_model_names:+ ($inactive_model_names)}"
    fi
    if [[ "$admin_state" == ready && "$active_model_count" -gt 0 ]]; then
        chat_state="configured (provider connectivity will be checked before completion)"
    else
        chat_state="incomplete"
    fi
    echo "  Chat:             $chat_state"
}

save_cli_api_url() {
    cli_api_prefix=""
    if "$cli" config set api_url "$ASTRA_API_URL" >/dev/null 2>&1; then
        ok "saved this installation as the CLI default"
    else
        warn "could not save the API URL in CLI settings"
        warn "prefix CLI commands with ASTRA_API_URL=$ASTRA_API_URL"
        printf -v cli_api_prefix 'ASTRA_API_URL=%q ' "$ASTRA_API_URL"
    fi
}

finish_infrastructure_only() {
    echo
    printf '\033[1;32mAstra stack is ready.\033[0m\n'
    echo "  API:   $ASTRA_API_URL"
    if [[ "${admin_state:-not configured}" == ready ]]; then
        echo "  Admin: administrator is ready; existing chat settings were kept"
    else
        echo "  Admin: not configured or not verified in this run"
    fi
    if (( ${active_model_count:-0} > 0 )); then
        echo "  Model: ${active_model_count} active model(s) present; connectivity was not rechecked"
    else
        echo "  Model: no active model configured"
    fi
    echo "  Resume: ${cli_api_prefix}${cli} admin setup"
    echo "  TUI:   ${cli_api_prefix}${cli}"
    echo "  Health: ${cli_api_prefix}${cli} health"
    if [[ -n "${stack_original_env:-}" ]]; then
        echo "  Existing installation descriptor kept at: $stack_original_env"
        printf '  Manage this installation with: '
        print_stack_command stack-status
        printf '\n'
    fi
    echo
    if [[ "${admin_state:-not configured}" == ready && "${active_model_count:-0}" -gt 0 ]]; then
        echo "Chat configuration is present; provider connectivity was not rechecked in this run."
    elif [[ "${admin_state:-not configured}" == ready ]]; then
        echo "Chat is not ready: add and activate an LLM model with astra admin setup."
    else
        echo "Chat is not ready: configure an administrator and an active model with astra admin setup."
    fi
    exit 0
}

run_admin_model_setup() {
    while true; do
        if "$cli" admin setup; then
            return 0
        fi
        warn "services are healthy, but administrator/model setup did not complete"
        printf '  Resume without restarting services: %s%q admin setup\n' \
            "$cli_api_prefix" "$cli" >&2
        choose "Choose how to continue:" \
            "Retry administrator and model setup" \
            "Finish the stack now and configure chat later" \
            "Exit with an error for inspection"
        case "$menu_choice" in
            1) continue ;;
            2) finish_infrastructure_only ;;
            3) return 1 ;;
        esac
    done
}

choose_admin_model_action() {
    if [[ "$admin_state" == ready && "$active_model_count" -gt 0 ]]; then
        choose "Administrator and model state is already present. What should setup do?" \
            "Verify the existing administrator and model (recommended)" \
            "Open setup to choose a different administrator or model" \
            "Finish the stack and leave chat settings unchanged"
        case "$menu_choice" in
            1|2) return 0 ;;
            3) finish_infrastructure_only ;;
        esac
    else
        choose "Chat setup is incomplete. What should setup do?" \
            "Configure administrator and model now (recommended)" \
            "Finish the stack and configure chat later"
        case "$menu_choice" in
            1) return 0 ;;
            2) finish_infrastructure_only ;;
        esac
    fi
}

echo
printf '\033[1;36mAstra local setup\033[0m\n'
echo "A guided setup for one local installation, memory, and optional chat configuration."
echo "No persistent data is removed by this wizard. Secrets are never displayed."

cli="$(resolve_cli)"

step "1/5" "Choosing the local installation and checking prerequisites"
make --no-print-directory stack-env STACK_ENV="$stack_env"
chmod 600 "$stack_env"
. scripts/lib/env_file.sh
embedding_endpoints="$(env_file_read "$stack_env" MEMORIA_EMBEDDING_ENDPOINTS 2>/dev/null || true)"
if [[ -n "$embedding_endpoints" ]]; then
    die "make stack-setup supports one embedding endpoint, but MEMORIA_EMBEDDING_ENDPOINTS is configured.
   Keep the advanced endpoint set and use make stack-start, or clear it before running the wizard."
fi
ensure_data_volume_is_not_shared
ok "Docker, Compose, Python, CLI, and local secrets are ready"
choose_existing_stack_action

step "2/5" "Configuring and testing semantic memory"
memory_changed=false
configure_local_memory_access
refresh_embedding_state
embedding_changed=false
show_embedding_state
if [[ "$(lower "$embedding_provider")" == mock ]] || {
    [[ -n "$embedding_url" ]] && [[ -n "$embedding_model" ]] &&
        [[ "$embedding_dimension" =~ ^[1-9][0-9]*$ ]];
}; then
    if ! confirm "Use this embedding configuration?" yes; then
        configure_embedding
        embedding_changed=true
    fi
else
    warn "embedding configuration is incomplete"
    configure_embedding
    embedding_changed=true
fi
refresh_embedding_state
probe_embedding
unset embedding_key
if [[ ( "$embedding_changed" == true || "$memory_changed" == true ) && "$reuse_stack" == true ]]; then
    warn "memory settings changed, so the existing containers must be recreated"
    reuse_stack=false
    startup_mode=repair
fi

step "3/5" "Reconciling and starting Astra services"
if [[ "$reuse_stack" == true ]]; then
    ok "healthy existing stack reused"
else
    if [[ "$startup_mode" == repair ]]; then
        start_stack true
    else
        start_stack false
    fi
    ok "MatrixOne, Memoria, and astra-server are running"
fi

step "4/5" "Verifying dependencies and optional user memory access"
verify_stack
ok "readiness and dependency memory round trip passed (see verification output for user-access coverage)"

step "5/5" "Reviewing optional administrator and model setup"
api_port="$(env_resolve_value "$stack_env" ASTRA_API_PORT 2>/dev/null || true)"
bind_address="$(env_resolve_value "$stack_env" ASTRA_BIND_ADDRESS 2>/dev/null || true)"
api_host="$(env_http_host_from_bind "$bind_address")"
export ASTRA_API_URL="http://${api_host}:${api_port:-17001}"
echo "  Model requests originate inside Docker. For a model server on this host,"
echo "  use http://host.docker.internal:<port> instead of localhost."
save_cli_api_url
show_setup_state
choose_admin_model_action
echo
echo "Continuing with administrator and model verification..."
run_admin_model_setup

echo
printf '\033[1;32mAstra is ready.\033[0m\n'
echo "  API:  $ASTRA_API_URL"
echo "  TUI:  ${cli_api_prefix}${cli}"
echo "  One-shot: ${cli_api_prefix}${cli} chat -m \"Hello Astra\""
echo "  Edge: astra-edge --help (connect a local runner when private tools are needed)"
if [[ -n "${stack_original_env:-}" ]]; then
    echo "  Env:  $stack_env"
    echo "  Existing installation descriptor kept at: $stack_original_env"
    printf '  Manage this installation: '
    print_stack_command stack-status
    printf '\n'
fi
