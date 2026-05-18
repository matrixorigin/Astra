#!/bin/bash
# Shared web agent server settings.

ASTRA_WEB_AGENT_DEFAULT_HOST=127.0.0.1
ASTRA_WEB_AGENT_DEFAULT_PORT=3536

web_agent_host() {
    printf '%s\n' "${ASTRA_WEB_HOST:-${WEB_HOST:-$ASTRA_WEB_AGENT_DEFAULT_HOST}}"
}

web_agent_port() {
    printf '%s\n' "${ASTRA_WEB_PORT:-${WEB_PORT:-$ASTRA_WEB_AGENT_DEFAULT_PORT}}"
}

web_validate_port() {
    local port=$1
    if [[ ! "$port" =~ ^[0-9]+$ ]] || [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        echo "Invalid web agent port: $port"
        echo "Set ASTRA_WEB_PORT or WEB_PORT to a value between 1 and 65535."
        return 1
    fi
}

web_next_dev_process_port() {
    local pid=$1
    local cmd
    cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
    if [[ "$cmd" =~ --port[[:space:]]+([0-9]+) ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
    elif [[ "$cmd" =~ -p[[:space:]]+([0-9]+) ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
    fi
}

web_port_listener_pids() {
    local port=$1
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null || true
    else
        true
    fi
}

web_port_accepts_connections() {
    local port=$1
    (echo >/dev/tcp/127.0.0.1/"$port") >/dev/null 2>&1
}
