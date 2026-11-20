# Shared database configuration and native container discovery for ops scripts.
# The selected env file is read as data, never evaluated as shell code.

ops_repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
. "$ops_repo_root/scripts/lib/container_runtime.sh"
. "$ops_repo_root/scripts/lib/mysql_protocol.sh"
ops_env_file="${ASTRA_OPS_ENV_FILE:-$ops_repo_root/.env}"
if [[ -n "${ASTRA_OPS_ENV_FILE:-}" && ! -f "$ops_env_file" ]]; then
    echo "Ops environment file not found: $ops_env_file" >&2
    exit 1
fi
container_select "$ops_env_file"

DB_HOST="$(env_resolve_value "$ops_env_file" MATRIXONE_HOST 2>/dev/null || true)"
DB_PORT="$(env_resolve_value "$ops_env_file" MATRIXONE_PORT 2>/dev/null || true)"
DB_USER="$(env_resolve_value "$ops_env_file" MATRIXONE_USER 2>/dev/null || true)"
DB_PASSWORD="$(env_resolve_value "$ops_env_file" MATRIXONE_PASSWORD 2>/dev/null || true)"
DB_NAME="$(env_resolve_value "$ops_env_file" ASTRA_DATABASE 2>/dev/null || true)"
DB_HOST="${DB_HOST:-localhost}"
DB_PORT="${DB_PORT:-6001}"
DB_USER="${DB_USER:-root}"
DB_NAME="${DB_NAME:-astra_runtime}"
if [[ -z "$DB_PASSWORD" ]]; then
    echo "MATRIXONE_PASSWORD is required (environment or ASTRA_OPS_ENV_FILE)." >&2
    exit 1
fi

ops_find_matrixone() {
    local project engine_error engine_status
    matrixone_container_ids=""
    if [[ "$DB_HOST" == localhost || "$DB_HOST" == 127.0.0.1 ]] && command -v "$container_engine" >/dev/null 2>&1; then
        if engine_error="$(container_check_engine 2>&1)"; then
            :
        else
            engine_status=$?
            if [[ "$engine_status" != 1 ]]; then
                printf '%s\n' "$engine_error" >&2
                return "$engine_status"
            fi
            # Discovery is optional when using the configured host endpoint.
            # An unavailable daemon must behave like an absent executable.
            # Invalid backend selections are still rejected before discovery.
            matrixone_container_count=0
            return 0
        fi
        project="$(env_file_read "$ops_env_file" ASTRA_STACK_NAME 2>/dev/null || true)"
        local filters=(--filter "label=$container_label.service=matrixone")
        [[ -z "$project" ]] || filters+=(--filter "label=$container_label.project=$project")
        matrixone_container_ids="$(container_run ps "${filters[@]}" --format '{{.ID}}')" || return
    fi
    matrixone_container_count="$(printf '%s\n' "$matrixone_container_ids" | sed '/^$/d' | wc -l | tr -d ' ')"
}

ops_database_command() {
    local tool="$1" client tls_mode
    shift
    if [[ "$matrixone_container_count" -gt 1 ]]; then
        echo "Multiple local MatrixOne Compose containers found; select one installation." >&2
        return 1
    fi
    if [[ "$matrixone_container_count" == 1 ]] &&
        container_run exec "$matrixone_container_ids" sh -c 'command -v "$1"' _ "$tool" >/dev/null 2>&1; then
        MYSQL_PWD="$DB_PASSWORD" container_run exec -i -e MYSQL_PWD "$matrixone_container_ids" \
            "$tool" -h127.0.0.1 -P6001 -u"$DB_USER" "$@"
        return
    fi
    # Published MatrixOne images need not include client programs. The host
    # fallback still uses the descriptor's exact endpoint and canonical TLS
    # policy, never another engine's container or a default Unix socket.
    client="$(env_resolve_value "$ops_env_file" ASTRA_MYSQL_CLIENT 2>/dev/null || true)"
    client="${client:-mysql}"
    command -v "$client" >/dev/null 2>&1 || { echo "A host MySQL client is required." >&2; return 1; }
    tls_mode="$(env_resolve_value "$ops_env_file" ASTRA_MYSQL_TLS_MODE 2>/dev/null || true)"
    ASTRA_MYSQL_CLIENT="$client" ASTRA_MYSQL_TLS_MODE="${tls_mode:-auto}" \
        MATRIXONE_HOST="$DB_HOST" MATRIXONE_PORT="$DB_PORT" MATRIXONE_USER="$DB_USER" \
        MATRIXONE_PASSWORD="$DB_PASSWORD" mysql_connection_args || return
    [[ "$tool" != mysql ]] || tool="$mysql_client"
    command -v "$tool" >/dev/null 2>&1 || { echo "Required database client is missing: $tool" >&2; return 1; }
    MYSQL_PWD="$DB_PASSWORD" "$tool" "${mysql_connection_options[@]}" "$@"
}
