#!/usr/bin/env bash
# Exercise retained release-candidate discovery without GitHub access.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-owner-artifacts.XXXXXX")"
trap 'rm -rf "${fixture_root}"' EXIT HUP INT TERM

fake_bin="${fixture_root}/bin"
mkdir -p "${fake_bin}"
cat > "${fake_bin}/gh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == api ]] || exit 90
[[ "${2:-}" == "repos/matrixorigin/Astra/actions/runs/12345/artifacts?per_page=100" ]] || exit 91
if [[ "${ASTRA_TEST_GH_FAILURE:-false}" == true ]]; then
    exit 1
fi
printf '%s\n' "${ASTRA_TEST_ARTIFACTS:-}"
SH
chmod 0755 "${fake_bin}/gh"

complete_artifacts=$'release-client-assets\nrelease-digest-linux-amd64\nrelease-digest-linux-arm64'
PATH="${fake_bin}:${PATH}" ASTRA_TEST_ARTIFACTS="${complete_artifacts}" \
    "${repo_root}/scripts/verify-release-owner-artifacts.sh" \
    matrixorigin/Astra 12345

if PATH="${fake_bin}:${PATH}" \
    ASTRA_TEST_ARTIFACTS=$'release-client-assets\nrelease-digest-linux-amd64' \
    "${repo_root}/scripts/verify-release-owner-artifacts.sh" \
    matrixorigin/Astra 12345 >/dev/null 2>&1; then
    echo "release recovery accepted an incomplete retained candidate set" >&2
    exit 1
fi

if PATH="${fake_bin}:${PATH}" \
    ASTRA_TEST_ARTIFACTS="${complete_artifacts}"$'\nrelease-client-assets' \
    "${repo_root}/scripts/verify-release-owner-artifacts.sh" \
    matrixorigin/Astra 12345 >/dev/null 2>&1; then
    echo "release recovery accepted an ambiguous retained candidate set" >&2
    exit 1
fi

if PATH="${fake_bin}:${PATH}" ASTRA_TEST_GH_FAILURE=true \
    "${repo_root}/scripts/verify-release-owner-artifacts.sh" \
    matrixorigin/Astra 12345 >/dev/null 2>&1; then
    echo "release recovery treated an artifact API failure as an empty candidate set" >&2
    exit 1
fi

if PATH="${fake_bin}:${PATH}" \
    "${repo_root}/scripts/verify-release-owner-artifacts.sh" \
    matrixorigin/Astra invalid >/dev/null 2>&1; then
    echo "release recovery accepted an invalid owner run ID" >&2
    exit 1
fi
