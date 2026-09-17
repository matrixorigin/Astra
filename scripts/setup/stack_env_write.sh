# Credential-safe environment writer and temporary-file lifecycle for
# stack-setup.sh. The caller installs cleanup_setup_temporary_files as its EXIT
# trap so cancellation cannot leave secret-bearing files behind.

stack_staging_env=""
stack_write_env_temp=""
stack_write_value_temp=""

cleanup_stack_write_files() {
    if [[ -n "${stack_write_env_temp:-}" ]]; then
        rm -f -- "$stack_write_env_temp" || true
        stack_write_env_temp=""
    fi
    if [[ -n "${stack_write_value_temp:-}" ]]; then
        rm -f -- "$stack_write_value_temp" || true
        stack_write_value_temp=""
    fi
}

cleanup_setup_temporary_files() {
    cleanup_stack_write_files
    if [[ -n "${stack_staging_env:-}" ]]; then
        rm -f -- "$stack_staging_env" || true
        stack_staging_env=""
    fi
}

set_env_value() {
    local key="$1" value="$2" target="${3:-$stack_env}"
    stack_write_env_temp="$(mktemp "${TMPDIR:-/tmp}/astra-stack-env.XXXXXX")" || return 1
    stack_write_value_temp="$(mktemp "${TMPDIR:-/tmp}/astra-stack-value.XXXXXX")" || {
        cleanup_stack_write_files
        return 1
    }
    if ! chmod 600 "$stack_write_value_temp" ||
        ! printf '%s' "$value" > "$stack_write_value_temp"; then
        cleanup_stack_write_files
        return 1
    fi
    if ! ASTRA_SETUP_VALUE_FILE="$stack_write_value_temp" awk -v key="$key" '
        BEGIN {
            value_file = ENVIRON["ASTRA_SETUP_VALUE_FILE"]
            if ((getline file_value < value_file) > 0) value = file_value
            close(value_file)
            updated = 0
        }
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            if (line ~ "^" key "[[:space:]]*=") {
                print key "=" value
                updated = 1
                next
            }
            print
        }
        END { if (!updated) print key "=" value }
    ' "$target" > "$stack_write_env_temp"; then
        cleanup_stack_write_files
        return 1
    fi
    if ! chmod 600 "$stack_write_env_temp"; then
        cleanup_stack_write_files
        return 1
    fi
    if ! mv "$stack_write_env_temp" "$target"; then
        cleanup_stack_write_files
        return 1
    fi
    stack_write_env_temp=""
    cleanup_stack_write_files
}
