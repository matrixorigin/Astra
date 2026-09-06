# Installation identity, isolation, port, and plan helpers for stack-setup.sh.
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

isolated_stack_env_path() {
    local source_env="$1" name="$2"
    printf '%s.%s.env' "$source_env" "$name"
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

port_is_reserved() {
    local port="$1" reserved="${2:-}"
    case " $reserved " in
        *" $port ") return 0 ;;
        *) return 1 ;;
    esac
}

next_available_port() {
    local bind_address="$1" port="$2" last_port reserved="${3:-}"
    port=$((10#$port))
    last_port=$((port + 99))
    ((last_port > 65535)) && last_port=65535
    while ((port <= last_port)); do
        if ! port_is_reserved "$port" "$reserved" &&
            port_is_available "$bind_address" "$port"; then
            printf '%s' "$port"
            return 0
        fi
        port=$((port + 1))
    done
    return 1
}

configure_isolated_stack() {
    local suggested name bind_address key current default port
    local old_stack_env isolated_env temporary_env
    local reserved_ports="" selected_ports=""
    suggested="$(suggest_isolated_stack_name)"
    echo
    echo "A separate installation keeps the existing containers and data untouched."
    echo "Setup will create a new MatrixOne volume and choose unused local ports."
    old_stack_env="$stack_env"
    while true; do
        read_default "Name for the separate installation" "$suggested"
        name="$(lower "$prompt_value")"
        case "$name" in
            ''|[!a-z0-9]*|*[!a-z0-9_-]*)
                warn "use lowercase letters, numbers, hyphens, or underscores; start with a letter or number"
                continue
                ;;
        esac
        isolated_env="$(isolated_stack_env_path "$old_stack_env" "$name")"
        if project_name_exists "$name" || volume_name_exists "${name}-matrixone-data" ||
            [[ -e "$isolated_env" ]]; then
            warn "an installation or retained data named '$name' already exists"
            continue
        fi
        break
    done

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
        port="$(next_available_port "$bind_address" "$((10#$current + 1))" "$reserved_ports")" ||
            die "no unused TCP port was found in the next 100 ports after $current for $key"
        selected_ports="${selected_ports}${selected_ports:+ }$port"
        reserved_ports="${reserved_ports}${reserved_ports:+ }$port"
    done

    # Build the complete descriptor in a sibling temporary file. The current
    # descriptor remains the source of truth until every value has been
    # validated and written, so an allocation or filesystem failure is safe to
    # retry and cannot strand the existing installation.
    temporary_env="$(mktemp "${isolated_env}.staging.XXXXXX")" ||
        die "could not stage the separate installation descriptor"
    stack_staging_env="$temporary_env"
    chmod 600 "$temporary_env"
    if ! cp "$old_stack_env" "$temporary_env"; then
        cleanup_setup_temporary_files
        die "could not copy the existing installation descriptor"
    fi
    if ! set_env_value ASTRA_STACK_NAME "$name" "$temporary_env" ||
        ! set_env_value MATRIXONE_DATA_VOLUME "${name}-matrixone-data" "$temporary_env" ||
        ! set_env_value MATRIXONE_LOG_DIR "./data/stacks/${name}/matrixone/logs" "$temporary_env" ||
        ! set_env_value MEMORIA_LOG_DIR "./data/stacks/${name}/memoria/logs" "$temporary_env"; then
        cleanup_setup_temporary_files
        die "could not stage the separate installation descriptor"
    fi
    for key in ASTRA_API_PORT MEMORIA_PORT MATRIXONE_PORT MATRIXONE_DEBUG_HTTP_PORT; do
        port="${selected_ports%% *}"
        selected_ports="${selected_ports#* }"
        [[ "$selected_ports" == "$port" ]] && selected_ports=""
        if ! set_env_value "$key" "$port" "$temporary_env"; then
            cleanup_setup_temporary_files
            die "could not stage the separate installation descriptor"
        fi
    done
    if ! mv "$temporary_env" "$isolated_env"; then
        cleanup_setup_temporary_files
        die "could not activate the separate installation descriptor"
    fi
    stack_staging_env=""
    if [[ -z "${stack_original_env:-}" ]]; then
        stack_original_env="$old_stack_env"
    fi
    stack_env="$isolated_env"

    echo "  New installation: $name"
    echo "  New data volume:  ${name}-matrixone-data"
    echo "  API port:         $(env_file_read "$stack_env" ASTRA_API_PORT)"
    echo "  Env file:         $stack_env"
    ok "separate installation staged atomically; the existing installation descriptor and data were not changed"
}

ensure_host_port() {
    local env_key="$1" label="$2" service="$3" default_port="$4" container_port="$5"
    local reserved_ports="${6:-}" bind_address port pid owner suggested answer duplicate
    bind_address="$(env_file_read "$stack_env" ASTRA_BIND_ADDRESS 2>/dev/null || true)"
    bind_address="${bind_address:-127.0.0.1}"
    port="$(env_file_read "$stack_env" "$env_key" 2>/dev/null || true)"
    port="${port:-$default_port}"
    case "$port" in
        ''|*[!0-9]*|0) die "$env_key must be a valid TCP port" ;;
    esac
    port=$((10#$port))
    ((port <= 65535)) || die "$env_key must be a TCP port from 1 to 65535"

    while true; do
        owner=""
        duplicate=false
        if port_is_reserved "$port" "$reserved_ports"; then
            duplicate=true
            warn "$label port $bind_address:$port is already assigned to another service in this stack"
        elif service_owns_host_port "$service" "$port" "$container_port"; then
            effective_host_port="$port"
            return 0
        elif port_is_available "$bind_address" "$port"; then
            effective_host_port="$port"
            return 0
        else
            pid="$(listener_pid "$port")"
            if [[ -n "$pid" ]]; then
                owner="$(process_name "$pid")"
            fi
            warn "$label port $bind_address:$port is already in use${owner:+ by $owner (PID $pid)}"
        fi

        if [[ "$duplicate" == true ]]; then
            choose "Resolve the duplicate $label port:" \
                "Use a different $label port" \
                "Exit without starting or changing containers"
            case "$menu_choice" in
                1)
                    suggested="$((port + 1))"
                    read_tcp_port "New $label port" "$suggested"
                    answer="$prompt_value"
                    set_env_value "$env_key" "$answer"
                    port="$answer"
                    continue
                    ;;
                2) echo "Existing services and data were left unchanged."; exit 0 ;;
            esac
        elif [[ "$env_key" == ASTRA_API_PORT && "$owner" == astra-server ]]; then
            choose "Resolve the API port conflict:" \
                "Stop the detected source-mode Astra API (PID $pid) and continue" \
                "Use a different all-in-one API port" \
                "Exit without further changes"
            case "$menu_choice" in
                1) stop_detected_api "$pid" "$port" || true; continue ;;
                2)
                    suggested="$((port + 1))"
                    read_tcp_port 'New all-in-one API port' "$suggested"
                    answer="$prompt_value"
                    set_env_value "$env_key" "$answer"
                    port="$answer"
                    continue
                    ;;
                3) echo "Existing services and data were left unchanged."; exit 0 ;;
            esac
        else
            choose "Resolve the $label port conflict:" \
                "Use a different $label port" \
                "Exit and stop the conflicting service yourself"
            case "$menu_choice" in
                1)
                    suggested="$((port + 1))"
                    read_tcp_port "New $label port" "$suggested"
                    answer="$prompt_value"
                    set_env_value "$env_key" "$answer"
                    port="$answer"
                    continue
                    ;;
                2) echo "Existing services and data were left unchanged."; exit 0 ;;
            esac
        fi
    done
}

check_host_ports() {
    local reserved_ports=""
    ensure_host_port ASTRA_API_PORT "API" api 17001 17001 "$reserved_ports"
    reserved_ports="$effective_host_port"
    ensure_host_port MEMORIA_PORT "Memoria" memoria 8100 8100 "$reserved_ports"
    reserved_ports="${reserved_ports}${reserved_ports:+ }$effective_host_port"
    ensure_host_port MATRIXONE_PORT "MatrixOne SQL" matrixone 26001 6001 "$reserved_ports"
    reserved_ports="${reserved_ports}${reserved_ports:+ }$effective_host_port"
    ensure_host_port MATRIXONE_DEBUG_HTTP_PORT "MatrixOne debug" matrixone 26060 6060 "$reserved_ports"
    ok "required host ports are unique and available or owned by this stack"
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
    if ! docker volume inspect "$volume_name" >/dev/null 2>&1; then
        return 0
    fi

    owner="$(docker volume inspect --format '{{index .Labels "com.docker.compose.project"}}' "$volume_name" 2>/dev/null || true)"
    if [[ "$owner" == "$stack_name" ]]; then
        return 0
    fi

    if [[ -z "$owner" || "$owner" == '<no value>' ]]; then
        warn "MatrixOne volume '$volume_name' already exists but has no trusted Compose owner"
    else
        warn "MatrixOne volume '$volume_name' belongs to the existing '$owner' installation"
    fi
    choose "Using it from '$stack_name' could fail or produce unexpected data. What should setup do?" \
        "Create a separate installation with new data and unused ports" \
        "Exit without starting or changing containers"
    case "$menu_choice" in
        1) configure_isolated_stack ;;
        2) echo "Existing services and data were left unchanged."; exit 0 ;;
    esac
}
