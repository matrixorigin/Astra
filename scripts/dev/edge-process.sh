# Shared ownership check for the astra-edge process managed by this checkout.

edge_process_is_owned() {
    edge_process_repo_root="$1"
    edge_process_pid="$2"
    edge_process_command_path=""

    case "$edge_process_pid" in
        ""|*[!0-9]*) return 1 ;;
    esac
    kill -0 "$edge_process_pid" 2>/dev/null || return 1

    if [ -r "/proc/${edge_process_pid}/cmdline" ]; then
        edge_process_command_path="$(tr '\0' '\n' < "/proc/${edge_process_pid}/cmdline" | sed -n '1p')"
    else
        edge_process_command_path="$(ps -p "$edge_process_pid" -o command= 2>/dev/null || true)"
    fi

    case "$edge_process_command_path" in
        "${edge_process_repo_root}/target/debug/astra-edge"|"${edge_process_repo_root}/target/debug/astra-edge "*|"${edge_process_repo_root}/target/release/astra-edge"|"${edge_process_repo_root}/target/release/astra-edge "*)
            return 0
            ;;
    esac
    return 1
}
