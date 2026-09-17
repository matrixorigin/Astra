#!/usr/bin/env bash
# Exercise run-scoped Docker candidate retention without a registry.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-candidate-tags.XXXXXX")"
trap 'rm -rf "${fixture_root}"' EXIT HUP INT TERM

fake_bin="${fixture_root}/bin"
state_dir="${fixture_root}/state"
mkdir -p "${fake_bin}" "${state_dir}"
candidate_raw='{"schemaVersion":2,"platform":"linux/amd64"}'
candidate_digest="sha256:$(printf '%s' "${candidate_raw}" | sha256sum | awk '{ print $1 }')"

cat > "${fake_bin}/docker" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == buildx && "${2:-}" == imagetools ]] || exit 90
operation="${3:-}"
shift 3
case "${operation}" in
    inspect)
        reference="${1:-}"
        if [[ "${reference}" == *"@${ASTRA_TEST_CANDIDATE_DIGEST}" ]]; then
            [[ "${ASTRA_TEST_SOURCE_MISSING:-false}" != true ]] || {
                echo 'manifest unknown' >&2
                exit 1
            }
            [[ "${2:-}" == --raw ]] || printf 'candidate\n'
            [[ "${2:-}" != --raw ]] || printf '%s' "${ASTRA_TEST_CANDIDATE_RAW}"
            exit 0
        fi
        if [[ ! -e "${ASTRA_TEST_STATE_DIR}/tag" ]]; then
            echo 'manifest unknown' >&2
            exit 1
        fi
        [[ "${2:-}" == --raw ]] || printf 'tag\n'
        if [[ "${2:-}" == --raw ]]; then
            if [[ "${ASTRA_TEST_TAG_MISMATCH:-false}" == true ]]; then
                printf '%s' '{"different":true}'
            else
                printf '%s' "${ASTRA_TEST_CANDIDATE_RAW}"
            fi
        fi
        ;;
    create)
        [[ "${1:-}" == --tag && -n "${2:-}" && -n "${3:-}" ]] || exit 91
        touch "${ASTRA_TEST_STATE_DIR}/tag"
        ;;
    *) exit 92 ;;
esac
SH
chmod 0755 "${fake_bin}/docker"

common_env=(
    "PATH=${fake_bin}:${PATH}"
    "ASTRA_TEST_STATE_DIR=${state_dir}"
    "ASTRA_TEST_CANDIDATE_RAW=${candidate_raw}"
    "ASTRA_TEST_CANDIDATE_DIGEST=${candidate_digest}"
)
reconciler="${repo_root}/scripts/reconcile-docker-candidate-tag.sh"
staging_tag="astra-candidate-123-linux-amd64-${candidate_digest#sha256:}"

env "${common_env[@]}" "${reconciler}" matrixorigin/astra \
    "${staging_tag}" "${candidate_digest}" create >/dev/null
# Repeating the same staging operation is idempotent and preserves the tag.
env "${common_env[@]}" "${reconciler}" matrixorigin/astra \
    "${staging_tag}" "${candidate_digest}" create >/dev/null
env "${common_env[@]}" "${reconciler}" matrixorigin/astra \
    "${staging_tag}" "${candidate_digest}" verify >/dev/null

if env "${common_env[@]}" ASTRA_TEST_TAG_MISMATCH=true "${reconciler}" \
    matrixorigin/astra "${staging_tag}" \
    "${candidate_digest}" verify >/dev/null 2>&1; then
    echo "candidate recovery accepted a staging tag with different content" >&2
    exit 1
fi

missing_state="${fixture_root}/missing-state"
mkdir -p "${missing_state}"
if env "${common_env[@]}" ASTRA_TEST_STATE_DIR="${missing_state}" "${reconciler}" \
    matrixorigin/astra "${staging_tag}" \
    "${candidate_digest}" verify >/dev/null 2>&1; then
    echo "candidate recovery accepted a missing run-scoped staging tag" >&2
    exit 1
fi

if env "${common_env[@]}" ASTRA_TEST_SOURCE_MISSING=true "${reconciler}" \
    matrixorigin/astra "${staging_tag}" \
    "${candidate_digest}" verify >/dev/null 2>&1; then
    echo "candidate recovery accepted a digest removed from the registry" >&2
    exit 1
fi
