# Prepare personal single-host model access through the caller's existing env
# writer. Never replace configured installation identity or operator settings.
initialize_personal_models() {
    personal_env_file="$1"
    personal_env_writer="$2"
    [ "${ASTRA_UC_ISSUER+x}" != x ] || return 0
    personal_uc_issuer="${ASTRA_UC_ISSUER-$(env_file_read "$personal_env_file" ASTRA_UC_ISSUER 2>/dev/null || true)}"
    [ -z "$personal_uc_issuer" ] || return 0
    personal_enabled="${ASTRA_CHATGPT_SUBSCRIPTION_ENABLED-$(env_file_read "$personal_env_file" ASTRA_CHATGPT_SUBSCRIPTION_ENABLED 2>/dev/null || true)}"
    if [ "${ASTRA_CHATGPT_SUBSCRIPTION_ENABLED+x}" = x ] && [ -z "$personal_enabled" ]; then return 0; fi
    if [ -z "$personal_enabled" ]; then
        "$personal_env_writer" ASTRA_CHATGPT_SUBSCRIPTION_ENABLED 1
        personal_enabled=1
    fi
    case "$personal_enabled" in 1|true) ;; *) return 0 ;; esac
    personal_host="${ASTRA_CHATGPT_HOST_ID-$(env_file_read "$personal_env_file" ASTRA_CHATGPT_HOST_ID 2>/dev/null || true)}"
    if [ "${ASTRA_CHATGPT_HOST_ID+x}" = x ] && [ -z "$personal_host" ]; then
        echo 'ASTRA_CHATGPT_HOST_ID is empty in the environment; restore the installation UUID.' >&2
        return 1
    fi
    if [ -n "$personal_host" ]; then
        printf '%s\n' "$personal_host" | grep -Eq '^urn:uuid:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$' || {
            echo 'Invalid ASTRA_CHATGPT_HOST_ID; restore the configured installation UUID.' >&2
            return 1
        }
        return 0
    fi
    personal_random="$(openssl rand -hex 16)" || {
        echo 'Unable to generate the installation UUID.' >&2
        return 1
    }
    printf '%s\n' "$personal_random" | grep -Eq '^[0-9a-f]{32}$' || return 1
    personal_host="$(printf '%s\n' "$personal_random" | awk '{
        variant=substr("89ab", (index("0123456789abcdef",substr($0,17,1))-1)%4+1, 1)
        printf "urn:uuid:%s-%s-4%s-%s%s-%s", substr($0,1,8),substr($0,9,4),substr($0,14,3),variant,substr($0,18,3),substr($0,21,12)
    }')" || return 1
    "$personal_env_writer" ASTRA_CHATGPT_HOST_ID "$personal_host"
}
