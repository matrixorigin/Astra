# Self-hosted memory setup. Existing opt-outs and custom image pins are never
# silently overwritten. The caller owns prompts and the env-file writer.

configure_local_memory_access() {
    local access website image supported_image
    access="$(env_file_read "$stack_env" MEMORIA_SELF_HOSTED_MASTER_ACCESS 2>/dev/null || true)"
    website="$(env_file_read "$stack_env" MEMORIA_WEB_URL 2>/dev/null || true)"
    if [[ -n "$website" ]]; then
        warn "Browser-login deployment: local memory fallback is unavailable; existing sharing permissions apply."
        return
    fi
    image="$(env_file_read "$stack_env" MEMORIA_IMAGE 2>/dev/null || true)"
    supported_image="$(env_file_read "$repo_root/deployment/all-in-one/.env.example" MEMORIA_IMAGE)"
    if [[ "$access" != 1 ]]; then
        warn "Local Astra users cannot access memory without explicit self-hosted authorization."
        if ! confirm "Enable owner-scoped memory for local password accounts (existing sharing permissions are preserved)?" no; then
            warn "Local user memory remains disabled; /memory will report permission denied."
            return
        fi
    fi
    if [[ "$image" != "$supported_image" ]]; then
        warn "The configured Memoria image must support Memoria-Owner authentication (available in 0.5.2)."
        if confirm "Use the Memoria image pinned by this release? Back up the database and check version compatibility first." no; then
            set_env_value MEMORIA_IMAGE "$supported_image"
            memory_changed=true
        elif ! confirm "Keep the custom image: have you verified that it supports Memoria-Owner authentication?" no; then
            die "Memory setup stopped. Select a compatible Memoria image before enabling local user memory."
        fi
    fi
    if [[ "$access" != 1 ]]; then
        set_env_value MEMORIA_SELF_HOSTED_MASTER_ACCESS 1
        memory_changed=true
    fi
    ok "Local password accounts use owner-scoped memory; existing sharing permissions remain authoritative"
}
