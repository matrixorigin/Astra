#!/usr/bin/env bash
# Create or verify a run-scoped tag that keeps one digest-addressed candidate reachable.

set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: $0 <image> <staging-tag> <sha256:digest> <create|verify>" >&2
    exit 2
fi

image_name="$1"
staging_tag="$2"
expected_digest="$3"
mode="$4"

[[ "${image_name}" != *[[:space:]]* && -n "${image_name}" ]] || {
    echo "image name must be one non-empty token" >&2
    exit 2
}
[[ "${staging_tag}" =~ ^[0-9A-Za-z_][0-9A-Za-z_.-]{0,127}$ ]] || {
    echo "invalid Docker staging tag: ${staging_tag}" >&2
    exit 2
}
[[ "${expected_digest}" =~ ^sha256:[0-9a-f]{64}$ ]] || {
    echo "invalid Docker candidate digest: ${expected_digest}" >&2
    exit 2
}
case "${mode}" in
    create|verify) ;;
    *)
        echo "candidate-tag mode must be create or verify" >&2
        exit 2
        ;;
esac

for command_name in docker sha256sum; do
    command -v "${command_name}" >/dev/null 2>&1 || {
        echo "${command_name} is required" >&2
        exit 1
    }
done

source_ref="${image_name}@${expected_digest}"
target_ref="${image_name}:${staging_tag}"
source_raw="$(mktemp "${RUNNER_TEMP:-/tmp}/astra-candidate-source.XXXXXX")"
target_raw="$(mktemp "${RUNNER_TEMP:-/tmp}/astra-candidate-target.XXXXXX")"
trap 'rm -f "${source_raw}" "${target_raw}"' EXIT HUP INT TERM

if ! docker buildx imagetools inspect "${source_ref}" --raw > "${source_raw}"; then
    echo "verified Docker candidate is no longer available: ${source_ref}" >&2
    exit 1
fi
source_digest="sha256:$(sha256sum "${source_raw}" | awk '{ print $1 }')"
if [[ "${source_digest}" != "${expected_digest}" ]]; then
    echo "Docker returned ${source_digest} for ${source_ref}" >&2
    exit 1
fi

target_exists=false
if inspect_output="$(docker buildx imagetools inspect "${target_ref}" 2>&1)"; then
    target_exists=true
elif ! grep -Eqi '(: not found|manifest unknown|name unknown|HTTP 404|status[^0-9]*404)' \
    <<< "${inspect_output}"; then
    echo "could not safely determine whether ${target_ref} exists:" >&2
    printf '%s\n' "${inspect_output}" >&2
    exit 1
fi

if [[ "${target_exists}" == false ]]; then
    if [[ "${mode}" == verify ]]; then
        echo "run-scoped Docker candidate tag is missing: ${target_ref}" >&2
        exit 1
    fi
    docker buildx imagetools create --tag "${target_ref}" "${source_ref}"
fi

if ! docker buildx imagetools inspect "${target_ref}" --raw > "${target_raw}"; then
    echo "could not read run-scoped Docker candidate tag: ${target_ref}" >&2
    exit 1
fi
target_digest="sha256:$(sha256sum "${target_raw}" | awk '{ print $1 }')"
if [[ "${target_digest}" != "${expected_digest}" ]]; then
    echo "${target_ref} resolves to ${target_digest}, expected ${expected_digest}" >&2
    exit 1
fi

echo "verified ${target_ref} -> ${expected_digest}"
