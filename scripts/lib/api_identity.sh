# Shared helpers for validating the source identity reported by Astra /health.

api_health_build_git_sha() {
    local health_body="${1:-}"
    if [ "$#" -eq 0 ]; then
        health_body=$(cat)
    fi
    printf '%s' "$health_body" |
        sed -nE 's/.*"build_git_sha"[[:space:]]*:[[:space:]]*"([0-9A-Fa-f]{40,64})".*/\1/p'
}

api_health_build_git_dirty() {
    local health_body="${1:-}"
    if [ "$#" -eq 0 ]; then
        health_body=$(cat)
    fi
    printf '%s' "$health_body" |
        sed -nE 's/.*"build_git_dirty"[[:space:]]*:[[:space:]]*(true|false).*/\1/p'
}

# Return success when a completed /health response cannot be used for this
# checkout. A response missing either identity field is an old/incompatible
# contract. An unavailable request returns failure so callers can keep waiting
# for a process that is still starting or briefly unreachable.
api_health_identity_mismatch() {
    local expected_sha="$1"
    local expected_dirty="$2"
    local health_body="$3"
    local actual_sha actual_dirty

    actual_sha=$(api_health_build_git_sha "$health_body")
    actual_dirty=$(api_health_build_git_dirty "$health_body")

    [ -n "$actual_sha" ] && [ -n "$actual_dirty" ] || return 0
    if [ "$actual_sha" = "$expected_sha" ] && [ "$actual_dirty" = "$expected_dirty" ]; then
        return 1
    fi
    return 0
}

api_health_identity_mismatch_from_url() {
    local expected_sha="$1"
    local expected_dirty="$2"
    local health_url="$3"
    local health_body

    health_body=$(NO_PROXY=localhost,127.0.0.1 curl -s \
        --connect-timeout 1 --max-time 2 "$health_url" 2>/dev/null) || return 1
    [ -n "$health_body" ] || return 1
    api_health_identity_mismatch "$expected_sha" "$expected_dirty" "$health_body"
}
