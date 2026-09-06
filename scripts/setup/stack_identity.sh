# Installation identity, isolation, and plan helpers for stack-setup.sh.
# The caller owns prompting, env-file access, and port probing primitives.

compose_plan_keeps_service() {
    local plan="$1"
    if grep -Eq ' (Create|Created|Recreate|Recreated|Remove|Removed)( |$)' <<< "$plan"; then
        return 1
    fi
    grep -Eq ' Container .* Running( |$)' <<< "$plan"
}

project_name_exists() {
    local name="$1"
    [[ -n "$(docker ps -a -q --filter "label=com.docker.compose.project=$name" 2>/dev/null)" ]]
}

volume_name_exists() {
    docker volume inspect "$1" >/dev/null 2>&1
}

suggest_isolated_stack_name() {
    local image version base candidate suffix=2
    image="$(env_resolve_value "$stack_env" ASTRA_IMAGE 2>/dev/null || true)"
    version="${image##*:}"
    if [[ -z "$version" || "$version" == "$image" || "$version" == *'@'* ]]; then
        version=local
    fi
    base="astra-$(printf '%s' "$version" | tr '[:upper:]./' '[:lower:]---' | tr -cd 'a-z0-9_-')"
    base="${base%-}"
    candidate="$base"
    while project_name_exists "$candidate" || volume_name_exists "${candidate}-matrixone-data"; do
        candidate="${base}-${suffix}"
        suffix=$((suffix + 1))
    done
    printf '%s' "$candidate"
}

next_available_port() {
    local bind_address="$1" port="$2" last_port
    port=$((10#$port))
    last_port=$((port + 99))
    ((last_port > 65535)) && last_port=65535
    while ((port <= last_port)); do
        if port_is_available "$bind_address" "$port"; then
            printf '%s' "$port"
            return 0
        fi
        port=$((port + 1))
    done
    return 1
}

configure_isolated_stack() {
    local suggested name bind_address key current default port
    suggested="$(suggest_isolated_stack_name)"
    echo
    echo "A separate installation keeps the existing containers and data untouched."
    echo "Setup will create a new MatrixOne volume and choose unused local ports."
    while true; do
        read_default "Name for the separate installation" "$suggested"
        name="$(lower "$prompt_value")"
        case "$name" in
            ''|[!a-z0-9]*|*[!a-z0-9_-]*)
                warn "use lowercase letters, numbers, hyphens, or underscores; start with a letter or number"
                continue
                ;;
        esac
        if project_name_exists "$name" || volume_name_exists "${name}-matrixone-data"; then
            warn "an installation or retained data named '$name' already exists"
            continue
        fi
        break
    done

    set_env_value ASTRA_STACK_NAME "$name"
    set_env_value MATRIXONE_DATA_VOLUME "${name}-matrixone-data"
    set_env_value MATRIXONE_LOG_DIR "./data/stacks/${name}/matrixone/logs"
    set_env_value MEMORIA_LOG_DIR "./data/stacks/${name}/memoria/logs"

    bind_address="$(env_file_read "$stack_env" ASTRA_BIND_ADDRESS 2>/dev/null || true)"
    bind_address="${bind_address:-127.0.0.1}"
    for key in ASTRA_API_PORT MEMORIA_PORT MATRIXONE_PORT MATRIXONE_DEBUG_HTTP_PORT; do
        case "$key" in
            ASTRA_API_PORT) default=17001 ;;
            MEMORIA_PORT) default=8100 ;;
            MATRIXONE_PORT) default=26001 ;;
            MATRIXONE_DEBUG_HTTP_PORT) default=26060 ;;
        esac
        current="$(env_file_read "$stack_env" "$key" 2>/dev/null || true)"
        current="${current:-$default}"
        case "$current" in
            ''|*[!0-9]*|0) die "$key must be a valid TCP port before creating a separate installation" ;;
        esac
        port="$(next_available_port "$bind_address" "$((10#$current + 1))")" ||
            die "no unused TCP port was found in the next 100 ports after $current for $key"
        set_env_value "$key" "$port"
    done

    echo "  New installation: $name"
    echo "  New data volume:  ${name}-matrixone-data"
    echo "  API port:         $(env_file_read "$stack_env" ASTRA_API_PORT)"
    ok "separate installation configured; the existing installation was not changed"
}

ensure_data_volume_is_not_shared() {
    local stack_name volume_name owner
    stack_name="$(env_file_read "$stack_env" ASTRA_STACK_NAME 2>/dev/null || true)"
    stack_name="${stack_name:-all-in-one}"
    volume_name="$(env_file_read "$stack_env" MATRIXONE_DATA_VOLUME 2>/dev/null || true)"
    volume_name="${volume_name:-astra-matrixone-data}"
    case "$stack_name" in
        ''|[!a-z0-9]*|*[!a-z0-9_-]*)
            die "ASTRA_STACK_NAME must start with a lowercase letter or number and contain only lowercase letters, numbers, hyphens, or underscores"
            ;;
    esac
    owner="$(docker volume inspect --format '{{index .Labels "com.docker.compose.project"}}' "$volume_name" 2>/dev/null || true)"
    if [[ -z "$owner" || "$owner" == '<no value>' || "$owner" == "$stack_name" ]]; then
        return 0
    fi

    warn "MatrixOne volume '$volume_name' belongs to the existing '$owner' installation"
    choose "Using it from '$stack_name' could fail or produce unexpected data. What should setup do?" \
        "Create a separate installation with new data and unused ports" \
        "Exit without starting or changing containers"
    case "$menu_choice" in
        1) configure_isolated_stack ;;
        2) echo "Existing services and data were left unchanged."; exit 0 ;;
    esac
}
