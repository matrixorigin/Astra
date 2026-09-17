#!/usr/bin/env bash
# Copy a verified image to an immutable tag without treating registry failures
# as proof that the target tag is absent.

set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <source-reference> <target-reference> <harbor-api-base-url>" >&2
    exit 2
fi

source_ref="$1"
target_ref="$2"
harbor_api_base_url="$3"

target_repository="${target_ref%:*}"
target_tag="${target_ref##*:}"
if [[ "${target_repository}" == "${target_ref}" || "${target_tag}" == */* || -z "${target_tag}" ]]; then
    echo "target reference must contain an explicit tag: ${target_ref}" >&2
    exit 2
fi
target_registry="${target_repository%%/*}"
target_repository_path="${target_repository#*/}"
if [[ "${target_registry}" == "${target_repository}" ]]; then
    echo "target reference must include a registry and repository: ${target_ref}" >&2
    exit 2
fi
if [[ "${harbor_api_base_url}" != "https://${target_registry}" && \
    "${harbor_api_base_url}" != "http://${target_registry}" ]]; then
    echo "Harbor API base URL must match the target registry" >&2
    exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v crane >/dev/null 2>&1 || {
    echo "crane is required" >&2
    exit 1
}

source_digest="$(crane digest "${source_ref}")"

target_exists=false
if target_digest="$(
    "${script_dir}/inspect-harbor-artifact.py" \
        "${harbor_api_base_url}" "${target_repository_path}" "${target_tag}"
)"; then
    target_exists=true
else
    lookup_status=$?
    if [[ "${lookup_status}" -ne 44 ]]; then
        echo "could not safely inspect ${target_ref} through the Harbor API" >&2
        exit 1
    fi
fi

if [[ "${target_exists}" == true ]]; then
    if [[ "${target_digest}" != "${source_digest}" ]]; then
        echo "${target_ref} already exists with digest ${target_digest}, expected ${source_digest}" >&2
        exit 1
    fi
    echo "verified existing immutable tag ${target_ref} -> ${source_digest}"
    exit 0
fi

crane copy --platform=all --jobs 2 "${source_ref}" "${target_ref}"
target_digest="$(crane digest "${target_ref}")"
if [[ "${target_digest}" != "${source_digest}" ]]; then
    echo "${target_ref} resolves to ${target_digest}, expected ${source_digest}" >&2
    exit 1
fi

echo "published immutable tag ${target_ref} -> ${source_digest}"
