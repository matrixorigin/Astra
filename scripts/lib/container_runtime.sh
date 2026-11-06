#!/usr/bin/env bash
# Native container backends for deployment scripts. Source for functions, or
# invoke: container_runtime.sh --env-file FILE --file COMPOSE compose up -d
# Empty array expansions preserve macOS Bash 3.2's nounset behavior.

container_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$container_lib_dir/env_file.sh"

container_select() {
    container_env_file="${1:-}"
    container_engine="${ASTRA_CONTAINER_ENGINE-}"
    if [[ ! ${ASTRA_CONTAINER_ENGINE+x} ]]; then
        container_engine="$(env_file_read "$container_env_file" ASTRA_CONTAINER_ENGINE 2>/dev/null || true)"
        container_engine="${container_engine:-docker}"
    fi
    case "$container_engine" in
        docker) container_command=(docker); container_compose_command=(docker compose); container_label=com.docker.compose ;;
        podman) container_command=(podman); container_compose_command=(podman-compose --in-pod=false --podman-run-args=--http-proxy=false); container_label=io.podman.compose ;;
        *) echo "ASTRA_CONTAINER_ENGINE must be docker or podman." >&2; return 2 ;;
    esac
}

container_run() {
    if [[ -n "${container_command_timeout:-}" ]]; then
        python3 "$container_lib_dir/container_state.py" run-timeout "$container_command_timeout" "${container_command[@]}" "$@"
    else
        "${container_command[@]}" "$@"
    fi
}

container_check_engine() {
    command -v "$container_engine" >/dev/null 2>&1 || {
        echo "Required container engine is missing: $container_engine" >&2; return 1;
    }
    local version
    version="$(container_run --version 2>/dev/null)" || return 1
    container_version="$version"
    if [[ "$container_engine" == docker && "$version" == *[Pp]odman* ]]; then
        echo "docker resolves to Podman. Select ASTRA_CONTAINER_ENGINE=podman for native support." >&2
        return 2
    fi
    container_run info >/dev/null 2>&1 || {
        echo "$container_engine is not accessible. Run this check from the host terminal." >&2; return 1;
    }
}

container_check() {
    container_check_engine || return
    if [[ "$container_engine" == podman ]]; then
        command -v podman-compose >/dev/null 2>&1 || {
            echo "Native Podman requires podman-compose (not docker-compose or podman compose)." >&2; return 1;
        }
        local version
        version="$(podman-compose version --short 2>/dev/null)" || return 1
        python3 - "$container_version" "$version" <<'PY' || return 1
import re
import sys
for label, version, minimum in (("Podman", sys.argv[1], (4, 6, 0)), ("podman-compose", sys.argv[2], (1, 6, 0))):
    match = re.search(r"\b(\d+)\.(\d+)\.(\d+)\b", version)
    if not match or tuple(map(int, match.groups())) < minimum:
        sys.exit("Native Podman requires " + label + " " + ".".join(map(str, minimum)) + " or newer.")
PY
    else
        "${container_compose_command[@]}" version >/dev/null 2>&1 || {
            echo "Docker Compose v2 is required." >&2; return 1;
        }
        "${container_compose_command[@]}" up --help 2>/dev/null | grep -q -- '--dry-run' || {
            echo "Docker Compose is too old for safe change planning; --dry-run is required." >&2; return 1;
        }
    fi
}

container_project() {
    local project="${container_project_name:-}"
    if [[ -z "$project" ]]; then
        project="$(env_file_read "$container_env_file" ASTRA_STACK_NAME 2>/dev/null || true)"
    fi
    if [[ -z "$project" ]]; then
        project="$(basename "$(dirname "${container_files[0]}")")"
    fi
    printf '%s\n' "$project"
}

container_prepare_compose() {
    local file key image rootless project
    project="$(container_project)"
    container_project_name="$project"
    container_compose_args=(--project-name "$project")
    for file in "${container_files[@]}"; do
        container_compose_args+=(--file "$file")
    done
    [[ -z "$container_env_file" ]] || container_compose_args+=(--env-file "$container_env_file")
    # Podman Compose hashes the env_file path, not its contents. Carry this
    # input dependency in the provider's own config hash instead of inventing
    # a second recreation algorithm or allowing stale API environment values.
    if [[ -f "$container_env_file" ]]; then
        ASTRA_STACK_ENV_HASH="$(python3 - "$container_env_file" <<'PY'
import hashlib
from pathlib import Path
import sys
print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)" || return
        export ASTRA_STACK_ENV_HASH
    fi
    if [[ "$container_engine" == podman ]]; then
        container_dependency_profile=false
        for file in "${container_files[@]}"; do
            if [[ "$(cd "$(dirname "$file")" && pwd)" == "$(cd "$container_lib_dir/../../deployment/all-in-one" && pwd)" ]]; then
                case "$(basename "$file")" in
                    docker-compose.yml|docker-compose.deps.yml)
                        container_dependency_profile=true
                        container_compose_args+=(--file "$container_lib_dir/../../deployment/all-in-one/docker-compose.podman-init.yml")
                        container_compose_args+=(--file "$container_lib_dir/../../deployment/all-in-one/docker-compose.podman-ready.yml")
                        ;;
                esac
            fi
        done
        rootless="$(container_run info --format '{{.Host.Security.Rootless}}')" || return
        # Map only the dependency containers and their init jobs to the host
        # user. The API retains its image's fixed, non-root runtime identity.
        if [[ "$rootless" == true ]]; then
            export ASTRA_DEPENDENCY_USERNS=keep-id
        fi
        # Keep engine-owned log paths independent even when an operator uses
        # the same descriptor with an explicit one-off engine override.
        for key in MATRIXONE_LOG_DIR MEMORIA_LOG_DIR; do
            if ! env_resolve_value "$container_env_file" "$key" >/dev/null 2>&1; then
                if [[ "$key" == MATRIXONE_LOG_DIR ]]; then
                    export MATRIXONE_LOG_DIR="./data/podman/$project/matrixone/logs"
                else
                    export MEMORIA_LOG_DIR="./data/podman/$project/memoria/logs"
                fi
            fi
        done
        # Podman must not prompt for registry short-name resolution. Preserve
        # the selected tags/digests while making Docker Hub references explicit.
        for key in ASTRA_IMAGE MATRIXONE_IMAGE MEMORIA_IMAGE NGINX_IMAGE; do
            image="$(env_resolve_value "$container_env_file" "$key" 2>/dev/null || true)"
            if [[ -n "$image" ]]; then
                if [[ "$image" != */* ]]; then
                    export "$key=docker.io/library/$image"
                else
                    case "${image%%/*}" in *.*|*:*|localhost) ;; *) export "$key=docker.io/$image" ;; esac
                fi
            fi
        done
    fi
}

container_initialize_dependencies() {
    local initialization_deadline="$1" service init available remaining
    shift
    local need_matrixone=false need_memoria=false
    [[ "${container_dependency_profile:-false}" == true && "$container_no_deps" == false ]] || return 0
    for service in "$@"; do
        case "$service" in
            matrixone|matrixone-init) need_matrixone=true ;;
            memoria|memoria-init|api) need_matrixone=true; need_memoria=true ;;
        esac
    done
    available="$(container_native_compose config --services)" || return
    for init in matrixone-init memoria-init; do
        if { [[ "$init" == matrixone-init && "$need_matrixone" == true ]] || [[ "$init" == memoria-init && "$need_memoria" == true ]]; } &&
            grep -qx "$init" <<< "$available"; then
            remaining=$((initialization_deadline - SECONDS))
            ((remaining > 0)) || { echo "Container initialization timed out." >&2; return 124; }
            container_command_timeout="$remaining" container_native_compose run --rm --no-deps -T "$init" || return
        fi
    done
}

container_native_compose() {
    local prefix=()
    if [[ -n "${container_command_timeout:-}" ]]; then
        prefix=(python3 "$container_lib_dir/container_state.py" run-timeout "$container_command_timeout")
    fi
    env -u COMPOSE_PROJECT_NAME -u COMPOSE_FILE \
        UID="$(id -u)" GID="$(id -g)" ASTRA_STACK_ENV_FILE="$container_env_file" \
        ${prefix[@]+"${prefix[@]}"} "${container_compose_command[@]}" "${container_compose_args[@]}" "$@"
}

container_ids() {
    local service="$1"
    container_run ps -a -q --filter "label=$container_label.project=$(container_project)" \
        --filter "label=$container_label.service=$service"
}

container_ps() {
    local args=(--filter "label=$container_label.project=$(container_project)") services=false
    while (($#)); do
        case "$1" in
            --status) args+=(--filter "status=${2:?Missing status}"); shift 2 ;;
            --services) services=true; shift ;;
            --format) args+=(--format "${2:?Missing format}"); shift 2 ;;
            -*) args+=("$1"); shift ;;
            *) args+=(--filter "label=$container_label.service=$1"); shift ;;
        esac
    done
    if [[ "$services" == true ]]; then
        local id
        for id in $(container_run ps -q ${args[@]+"${args[@]}"}); do
            container_run inspect "$id" | python3 "$container_lib_dir/container_state.py" service "$container_label.service" || return
        done
    else
        container_run ps ${args[@]+"${args[@]}"}
    fi
}

container_metadata() {
    local action="$1" id="$2" inspection
    inspection="$(container_run inspect "$id")" || return
    printf '%s' "$inspection" | python3 "$container_lib_dir/container_state.py" "$action"
}

container_probe_before() {
    local remaining=$(( $1 - SECONDS ))
    shift
    ((remaining > 0)) || { echo "Container readiness timeout expired." >&2; return 124; }
    container_command_timeout="$remaining" "$@"
}

container_wait() {
    local timeout="$1" service ids id status count expected specification deadline pending
    shift
    [[ "$timeout" =~ ^[1-9][0-9]*$ ]] || { echo "Readiness timeout must be a positive integer." >&2; return 2; }
    deadline=$((SECONDS + timeout))
    while :; do
        pending=false
        for service in "$@"; do
            expected=1
            for specification in ${container_scales[@]+"${container_scales[@]}"}; do
                [[ "${specification%%=*}" != "$service" ]] || expected="${specification#*=}"
            done
            [[ "$expected" != 0 ]] || continue
            ids="$(container_probe_before "$deadline" container_ids "$service")" || return
            count=0
            for id in $ids; do
                count=$((count + 1))
                status="$(container_probe_before "$deadline" container_metadata state "$id")" || return
                case "$status" in
                    healthy|running|completed) ;;
                    exited|dead|unhealthy|removing)
                        echo "Service $service failed readiness ($status)." >&2; return 1 ;;
                    *) pending=true ;;
                esac
            done
            [[ "$count" -ge "$expected" ]] || pending=true
        done
        if (( SECONDS >= deadline )); then
            echo "Services did not become ready within ${timeout}s: $*" >&2
            return 124
        fi
        [[ "$pending" == true ]] || return 0
        sleep 1
    done
}

container_podman_down() {
    local original=("$@") services=() stop_args=() selected_ids=()
    local available service ids id status removal_order
    # Validate with the provider before stopping anything. Its down command
    # otherwise only reconstructs the default replica count from YAML.
    if ! container_native_compose --dry-run down "$@" >/dev/null 2>&1; then
        echo "Cannot validate Podman Compose down arguments." >&2
        return 2
    fi
    while (($#)); do
        case "$1" in
            --timeout|-t) stop_args=(--time "$2"); shift 2 ;;
            --timeout=*) stop_args=(--time "${1#*=}"); shift ;;
            --rmi) shift 2 ;;
            --rmi=*|--volumes|-v|--remove-orphans) shift ;;
            --help|-h) container_native_compose down ${original[@]+"${original[@]}"}; return ;;
            -*) container_native_compose down ${original[@]+"${original[@]}"}; return ;;
            *) services+=("$1"); shift ;;
        esac
    done
    available="$(container_native_compose config --services)" || return
    if [[ ! ${services[@]+defined} ]]; then
        while IFS= read -r service; do [[ -z "$service" ]] || services+=("$service"); done <<< "$available"
    fi
    for service in ${services[@]+"${services[@]}"}; do
        grep -qx -- "$service" <<< "$available" || { echo "Unknown Compose service." >&2; return 2; }
    done
    for service in ${services[@]+"${services[@]}"}; do
        ids="$(container_ids "$service")" || return
        for id in $ids; do selected_ids+=("$id"); done
    done
    if [[ ${selected_ids[@]+defined} ]]; then
        # Native --requires references must be removed before their targets.
        # Use the runtime's actual graph, including every scaled replica,
        # rather than relying on YAML service order or removing dependencies
        # outside the selected installation/services.
        removal_order="$(container_run inspect "${selected_ids[@]}" |
            python3 "$container_lib_dir/container_state.py" removal-order)" || return
        while IFS= read -r id; do
            status="$(container_metadata state "$id")" || return
            case "$status" in
                running|healthy|starting|unhealthy|restarting|paused)
                    container_run stop --ignore ${stop_args[@]+"${stop_args[@]}"} "$id" || return ;;
            esac
            container_run rm --ignore "$id" || return
        done <<< "$removal_order"
    fi
    # Let the canonical provider remove networks and optionally volumes/images.
    # Missing default replica names are expected after the scoped removal above.
    container_native_compose --podman-stop-args=--ignore --podman-rm-args=--ignore down ${original[@]+"${original[@]}"}
}

container_compose() (
    # Settings for the provider stay local to this invocation, not the caller.
    container_prepare_compose || exit
    local action="${1:-}" argument wait=false timeout=180 deadline remaining
    local container_no_deps=false
    local args=() services=()
    container_scales=()
    shift
    if [[ "$action" == ps ]]; then
        container_ps "$@"
    elif [[ "$action" == down && "$container_engine" == podman ]]; then
        container_podman_down "$@"
    elif [[ "$action" == up ]]; then
        while (($#)); do
            argument="$1"; shift
            case "$argument" in
                --wait) wait=true ;;
                --wait-timeout) timeout="${1:?Missing wait timeout}"; shift ;;
                --wait-timeout=*) timeout="${argument#*=}" ;;
                --scale) container_scales+=("${1:?Missing scale}"); args+=(--scale "$1"); shift ;;
                --scale=*) container_scales+=("${argument#*=}"); args+=("$argument") ;;
                --no-deps) container_no_deps=true; args+=("$argument") ;;
                --pull|--timeout|--attach|--no-attach|--exit-code-from|--build-arg) args+=("$argument" "${1:?Missing argument}"); shift ;;
                -*) args+=("$argument") ;;
                *) services+=("$argument") ;;
            esac
        done
        [[ "$timeout" =~ ^[1-9][0-9]*$ ]] || { echo "Invalid readiness timeout." >&2; exit 2; }
        for argument in ${container_scales[@]+"${container_scales[@]}"}; do
            [[ "$argument" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.-]*=(0|[1-9][0-9]*)$ ]] || {
                echo "Invalid service scale." >&2; exit 2;
            }
        done
        if [[ ! ${services[@]+defined} ]] && { [[ "$wait" == true ]] || [[ "${container_dependency_profile:-false}" == true ]]; }; then
            local resolved
            resolved="$(container_native_compose config --services)" || exit
            [[ -n "$resolved" ]] || { echo "No services were resolved for startup." >&2; exit 1; }
            while IFS= read -r argument; do services+=("$argument"); done <<< "$resolved"
        fi
        deadline=$((SECONDS + timeout))
        container_initialize_dependencies "$deadline" ${services[@]+"${services[@]}"} || exit
        if [[ "${container_dependency_profile:-false}" == true ]]; then
            local filtered=()
            for argument in ${services[@]+"${services[@]}"}; do
                case "$argument" in matrixone-init|memoria-init) ;; *) filtered+=("$argument") ;; esac
            done
            services=(${filtered[@]+"${filtered[@]}"})
            [[ ${services[@]+defined} ]] || exit 0
        fi
        if [[ "$wait" == true ]]; then
            remaining=$((deadline - SECONDS))
            ((remaining > 0)) || { echo "Container initialization exhausted its readiness timeout." >&2; exit 124; }
            container_command_timeout="$remaining" container_native_compose up -d ${args[@]+"${args[@]}"} ${services[@]+"${services[@]}"} || exit
            remaining=$((deadline - SECONDS))
            ((remaining > 0)) || { echo "Container startup exhausted its readiness timeout." >&2; exit 124; }
            container_wait "$remaining" ${services[@]+"${services[@]}"}
        else
            container_native_compose up ${args[@]+"${args[@]}"} ${services[@]+"${services[@]}"}
        fi
    else
        container_native_compose "$action" "$@"
    fi
)

container_plan_matches() (
    container_prepare_compose || exit
    local service="$1" ids id name output
    ids="$(container_ids "$service")" || exit
    [[ -n "$ids" && "$ids" != *$'\n'* ]] || exit 1
    id="$ids"
    name="$(container_metadata name "$id")" || exit
    if [[ "$container_engine" == podman ]]; then
        output="$(container_native_compose --verbose --dry-run up -d --no-deps --no-build --pull never "$service" 2>&1)" || exit 1
    else
        output="$(COMPOSE_ANSI=never container_native_compose --dry-run up -d --no-deps --no-build --pull never "$service" 2>&1)" || exit 1
    fi
    printf '%s' "$output" | python3 "$container_lib_dir/container_state.py" plan "$container_engine" "$name"
)

container_main() {
    local action env_file="" file
    container_files=()
    container_project_name=""
    while (($#)); do
        case "$1" in
            --env-file) env_file="${2:?Missing env file}"; shift 2 ;;
            --file) file="${2:?Missing Compose file}"; container_files+=("$file"); shift 2 ;;
            --project-name) container_project_name="${2:?Missing project name}"; shift 2 ;;
            *) break ;;
        esac
    done
    if [[ -n "$env_file" && "$env_file" != /* ]]; then env_file="$PWD/$env_file"; fi
    container_select "$env_file" || return
    action="${1:-}"; (($# == 0)) || shift
    case "$action" in
        engine) printf '%s\n' "$container_engine" ;;
        check) container_check ;;
        native) container_check_engine && container_run "$@" ;;
        compose|plan)
            [[ ${container_files[@]+defined} ]] || { echo "A Compose --file is required." >&2; return 2; }
            container_check || return
            if [[ "$action" == plan ]]; then container_plan_matches "$@"; else container_compose "$@"; fi
            ;;
        *) echo "Usage: container_runtime.sh [--env-file FILE] [--file COMPOSE] [--project-name NAME] engine|check|native|compose|plan ..." >&2; return 2 ;;
    esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    set -euo pipefail
    container_main "$@"
fi
