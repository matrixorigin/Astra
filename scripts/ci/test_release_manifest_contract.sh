#!/usr/bin/env bash
# Exercise Docker manifest publication and recovery without a registry.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-manifest-contract.XXXXXX")"
trap 'rm -rf "${fixture_root}"' EXIT HUP INT TERM

fake_bin="${fixture_root}/bin"
digest_dir="${fixture_root}/digests"
mkdir -p "${fake_bin}" "${digest_dir}"

amd64_candidate="$(printf 'a%.0s' {1..64})"
arm64_candidate="$(printf 'b%.0s' {1..64})"
amd64_image="sha256:$(printf 'c%.0s' {1..64})"
arm64_image="sha256:$(printf 'd%.0s' {1..64})"
touch "${digest_dir}/${amd64_candidate}" "${digest_dir}/${arm64_candidate}"

cat > "${fake_bin}/docker" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

[[ "${1:-}" == buildx && "${2:-}" == imagetools ]] || exit 90
operation="${3:-}"
shift 3

case "${operation}" in
    create)
        [[ "${1:-}" == --tag && -n "${2:-}" ]] || exit 91
        touch "${ASTRA_TEST_DOCKER_STATE}/target"
        ;;
    inspect)
        reference="${1:-}"
        if [[ "${reference}" == *"@sha256:${ASTRA_TEST_AMD64_CANDIDATE}" ]]; then
            architecture=amd64
            image_digest="${ASTRA_TEST_AMD64_IMAGE}"
        elif [[ "${reference}" == *"@sha256:${ASTRA_TEST_ARM64_CANDIDATE}" ]]; then
            architecture="${ASTRA_TEST_SECOND_ARCH:-arm64}"
            image_digest="${ASTRA_TEST_ARM64_IMAGE}"
        else
            [[ -e "${ASTRA_TEST_DOCKER_STATE}/target" ]] || exit 1
            if [[ "${2:-}" == --format ]]; then
                printf '{"schemaVersion":2,"manifests":[{"digest":"%s","platform":{"os":"linux","architecture":"amd64"}},{"digest":"%s","platform":{"os":"linux","architecture":"arm64"}}]}\n' \
                    "${ASTRA_TEST_AMD64_IMAGE}" "${ASTRA_TEST_TARGET_ARM64_IMAGE:-${ASTRA_TEST_ARM64_IMAGE}}"
            else
                printf 'mock manifest\n'
            fi
            exit 0
        fi

        if [[ "${2:-}" == --format ]]; then
            printf '{"schemaVersion":2,"manifests":[{"digest":"%s","platform":{"os":"linux","architecture":"%s"}},{"digest":"sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","annotations":{"vnd.docker.reference.type":"attestation-manifest"},"platform":{"os":"unknown","architecture":"unknown"}}]}\n' \
                "${image_digest}" "${architecture}"
        else
            printf 'mock candidate\n'
        fi
        ;;
    *) exit 92 ;;
esac
SH
chmod 0755 "${fake_bin}/docker"

matrix='{"include":[{"platform":"linux/amd64"},{"platform":"linux/arm64"}]}'
common_env=(
    "PATH=${fake_bin}:${PATH}"
    "ASTRA_TEST_AMD64_CANDIDATE=${amd64_candidate}"
    "ASTRA_TEST_ARM64_CANDIDATE=${arm64_candidate}"
    "ASTRA_TEST_AMD64_IMAGE=${amd64_image}"
    "ASTRA_TEST_ARM64_IMAGE=${arm64_image}"
)

success_state="${fixture_root}/success-state"
mkdir -p "${success_state}"
env "${common_env[@]}" ASTRA_TEST_DOCKER_STATE="${success_state}" \
    "${repo_root}/scripts/reconcile-docker-manifest.sh" \
    matrixorigin/astra 0.1.0 "${matrix}" "${digest_dir}" reject >/dev/null

# Recovery may reuse exactly matching immutable output.
env "${common_env[@]}" ASTRA_TEST_DOCKER_STATE="${success_state}" \
    "${repo_root}/scripts/reconcile-docker-manifest.sh" \
    matrixorigin/astra 0.1.0 "${matrix}" "${digest_dir}" verify >/dev/null

# A normal publication must never overwrite a tag, even if its content matches.
if env "${common_env[@]}" ASTRA_TEST_DOCKER_STATE="${success_state}" \
    "${repo_root}/scripts/reconcile-docker-manifest.sh" \
    matrixorigin/astra 0.1.0 "${matrix}" "${digest_dir}" reject >/dev/null 2>&1; then
    echo "manifest contract overwrote an existing immutable tag" >&2
    exit 1
fi

# Two digest files are insufficient: their actual platforms must match the matrix.
wrong_state="${fixture_root}/wrong-state"
mkdir -p "${wrong_state}"
if env "${common_env[@]}" ASTRA_TEST_DOCKER_STATE="${wrong_state}" \
    ASTRA_TEST_SECOND_ARCH=amd64 \
    "${repo_root}/scripts/reconcile-docker-manifest.sh" \
    matrixorigin/astra 0.1.0 "${matrix}" "${digest_dir}" reject >/dev/null 2>&1; then
    echo "manifest contract accepted duplicate AMD64 candidates" >&2
    exit 1
fi
[[ ! -e "${wrong_state}/target" ]]

# Recovery must reject an existing tag whose platform digest differs.
mismatch_state="${fixture_root}/mismatch-state"
mkdir -p "${mismatch_state}"
touch "${mismatch_state}/target"
mismatch_digest="sha256:$(printf 'f%.0s' {1..64})"
if env "${common_env[@]}" ASTRA_TEST_DOCKER_STATE="${mismatch_state}" \
    ASTRA_TEST_TARGET_ARM64_IMAGE="${mismatch_digest}" \
    "${repo_root}/scripts/reconcile-docker-manifest.sh" \
    matrixorigin/astra 0.1.0 "${matrix}" "${digest_dir}" verify >/dev/null 2>&1; then
    echo "manifest recovery accepted different immutable output" >&2
    exit 1
fi
