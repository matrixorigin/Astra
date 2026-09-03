# Shared, non-evaluating helpers for the simple KEY=VALUE files used by Astra.
# This file is sourced by Bash and POSIX shell entrypoints; keep it portable.

env_file_read() {
    env_file_path="$1"
    env_file_key="$2"
    awk -v key="$env_file_key" '
        function quote_is_escaped(text, position,    count, cursor) {
            count = 0
            for (cursor = position - 1; cursor > 0; cursor--) {
                if (substr(text, cursor, 1) != "\\") break
                count++
            }
            return count % 2
        }

        /^[[:space:]]*#/ { next }
        {
            line = $0
            sub(/\r$/, "", line)
            sub(/^[[:space:]]*/, "", line)
            if (line ~ "^" key "[[:space:]]*=") {
                found = 0
                value = ""
                sub(/^[^=]*=/, "", line)
                sub(/^[[:space:]]*/, "", line)
                sub(/[[:space:]]*$/, "", line)
                first = substr(line, 1, 1)
                if (first == "\"" || first == "\047") {
                    closed = 0
                    for (i = 2; i <= length(line); i++) {
                        if (substr(line, i, 1) == first && !quote_is_escaped(line, i)) {
                            value = substr(line, 2, i - 2)
                            remainder = substr(line, i + 1)
                            sub(/^[[:space:]]*/, "", remainder)
                            if (remainder != "" && remainder !~ /^#/) break
                            closed = 1
                            break
                        }
                    }
                    if (!closed) next
                } else {
                    if (line ~ /^#/) {
                        line = ""
                    } else {
                        sub(/[[:space:]]+#.*/, "", line)
                    }
                    sub(/[[:space:]]*$/, "", line)
                    value = line
                }
                found = 1
            }
        }
        END {
            if (!found) exit 1
            print value
        }
    ' "$env_file_path"
}

env_value_is_placeholder() {
    env_file_lower_value="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
    case "$env_file_lower_value" in
        ""|*changeme*|*change_me*|*change-me*|*change_in_production*|*change-in-production*|your-*|*your-domain*|astra-dev-*|dev-master-key*|*placeholder*)
            return 0
            ;;
    esac
    return 1
}

env_file_has_configured_value() {
    env_file_value="$(env_file_read "$1" "$2" 2>/dev/null || true)"
    ! env_value_is_placeholder "$env_file_value"
}

# Match Docker Compose precedence: an exported process value (including an
# explicitly empty one) overrides the value in the env file.
env_resolve_value() {
    env_resolve_file="$1"
    env_resolve_key="$2"
    if printenv "$env_resolve_key" >/dev/null 2>&1; then
        printenv "$env_resolve_key"
    else
        env_file_read "$env_resolve_file" "$env_resolve_key"
    fi
}
