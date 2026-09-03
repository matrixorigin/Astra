#!/usr/bin/env bash
# Exercise the release metadata, archive-set, and installer contracts locally.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-release-contract.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT HUP INT TERM

package_dir="${fixture_root}/package"
dist_dir="${fixture_root}/dist"
fake_bin_dir="${fixture_root}/fake-bin"
install_dir="${fixture_root}/installed"
mkdir -p "$package_dir" "$dist_dir" "$fake_bin_dir" "$install_dir"
printf '%s\n' '#!/bin/sh' 'echo "astra 0.1.0"' > "${package_dir}/astra"
printf '%s\n' '#!/bin/sh' 'echo "astra-edge 0.1.0"' > "${package_dir}/astra-edge"
chmod 0755 "${package_dir}/astra" "${package_dir}/astra-edge"
install -m 0644 "${repo_root}/LICENSE" "${package_dir}/LICENSE"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

for suffix in linux-amd64 linux-arm64 darwin-amd64 darwin-arm64; do
    archive="astra-v0.1.0-${suffix}.tar.gz"
    tar -czf "${dist_dir}/${archive}" -C "$package_dir" astra astra-edge LICENSE
    printf '%s  %s\n' "$(sha256_file "${dist_dir}/${archive}")" "$archive" \
        > "${dist_dir}/${archive}.sha256"
done

"${repo_root}/scripts/verify-release-artifacts.sh" 0.1.0 "$dist_dir"
expected_manifest_hash="$(awk '{ print $1 }' "${dist_dir}/checksums.txt.sha256")"
actual_manifest_hash="$(sha256_file "${dist_dir}/checksums.txt")"
[[ "$expected_manifest_hash" = "$actual_manifest_hash" ]]

# Exercise the actual installer without network access. Its curl replacement
# serves the exact release fixture selected by the installer's platform logic.
printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'destination=""' \
    'url=""' \
    'while [ "$#" -gt 0 ]; do' \
    '  case "$1" in' \
    '    -o) destination="$2"; shift 2 ;;' \
    '    -*) shift ;;' \
    '    *) url="$1"; shift ;;' \
    '  esac' \
    'done' \
    'case "$url" in' \
    '  */releases/latest) printf '\''{"tag_name":"v0.1.0"}\n'\''; exit 0 ;;' \
    'esac' \
    'cp "${ASTRA_TEST_RELEASE_DIR}/${url##*/}" "$destination"' \
    > "${fake_bin_dir}/curl"
chmod 0755 "${fake_bin_dir}/curl"
PATH="${fake_bin_dir}:${PATH}" \
    ASTRA_TEST_RELEASE_DIR="$dist_dir" \
    ASTRA_INSTALL_DIR="$install_dir" \
    "${repo_root}/scripts/install-astra.sh" --version 0.1.0 >/dev/null
[[ "$("${install_dir}/astra" --version)" = "astra 0.1.0" ]]
[[ "$("${install_dir}/astra-edge" --version)" = "astra-edge 0.1.0" ]]

latest_output="$(
    PATH="${fake_bin_dir}:${PATH}" \
        ASTRA_TEST_RELEASE_DIR="$dist_dir" \
        ASTRA_INSTALL_DIR="$install_dir" \
        "${repo_root}/scripts/install-astra.sh" --dry-run
)"
grep -Fq "Release: v0.1.0" <<< "$latest_output"
grep -Fq "git clone --branch v0.1.0 --depth 1" <<< "$latest_output"
grep -Fq "make stack-setup" <<< "$latest_output"

for invalid_version in 01.0.0 0.1.0-01 0.1.0-rc..1; do
    if "${repo_root}/scripts/install-astra.sh" \
        --version "${invalid_version}" --dry-run >/dev/null 2>&1; then
        echo "installer accepted invalid semantic version ${invalid_version}" >&2
        exit 1
    fi
    if "${repo_root}/scripts/validate-release-version.sh" \
        "${invalid_version}" --syntax-only >/dev/null 2>&1; then
        echo "release preflight accepted invalid semantic version ${invalid_version}" >&2
        exit 1
    fi
done
"${repo_root}/scripts/validate-release-version.sh" \
    0.1.0-rc.1 --syntax-only >/dev/null

# If replacing the second binary fails, the installer must restore the previous
# matching CLI/Runner pair instead of leaving a mixed installation behind.
rollback_install_dir="${fixture_root}/rollback-installed"
rollback_marker="${fixture_root}/fail-cli-move-once"
mkdir -p "$rollback_install_dir"
printf '%s\n' '#!/bin/sh' 'echo "astra old"' > "${rollback_install_dir}/astra"
printf '%s\n' '#!/bin/sh' 'echo "astra-edge old"' > "${rollback_install_dir}/astra-edge"
chmod 0755 "${rollback_install_dir}/astra" "${rollback_install_dir}/astra-edge"
real_mv="$(command -v mv)"
cat > "${fake_bin_dir}/mv" <<'SH'
#!/bin/sh
set -eu
case "${2:-}" in
    */.astra-install.*/astra)
        if [ ! -e "${ASTRA_TEST_MV_MARKER}" ]; then
            : > "${ASTRA_TEST_MV_MARKER}"
            exit 1
        fi
        ;;
esac
exec "${ASTRA_TEST_REAL_MV}" "$@"
SH
chmod 0755 "${fake_bin_dir}/mv"
if PATH="${fake_bin_dir}:${PATH}" \
    ASTRA_TEST_RELEASE_DIR="$dist_dir" \
    ASTRA_INSTALL_DIR="$rollback_install_dir" \
    ASTRA_TEST_MV_MARKER="$rollback_marker" \
    ASTRA_TEST_REAL_MV="$real_mv" \
    "${repo_root}/scripts/install-astra.sh" --version 0.1.0 >/dev/null 2>&1; then
    echo "installer unexpectedly succeeded after a simulated commit failure" >&2
    exit 1
fi
[[ "$("${rollback_install_dir}/astra" --version)" = "astra old" ]]
[[ "$("${rollback_install_dir}/astra-edge" --version)" = "astra-edge old" ]]

# A checksum sidecar is one exact record, not a file whose first line happens
# to be valid.
checksum_fixture="${dist_dir}/astra-v0.1.0-darwin-amd64.tar.gz.sha256"
printf '%s\n' 'unexpected second record' >> "$checksum_fixture"
if "${repo_root}/scripts/verify-release-artifacts.sh" 0.1.0 "$dist_dir" >/dev/null 2>&1; then
    echo "release artifact verification accepted an ambiguous checksum file" >&2
    exit 1
fi
printf '%s  %s\n' \
    "$(sha256_file "${dist_dir}/astra-v0.1.0-darwin-amd64.tar.gz")" \
    'astra-v0.1.0-darwin-amd64.tar.gz' > "$checksum_fixture"

# The GitHub Release uploads dist/*, so unrecognized files must not pass the
# pre-publication gate.
printf '%s\n' unexpected > "${dist_dir}/unexpected.txt"
if "${repo_root}/scripts/verify-release-artifacts.sh" 0.1.0 "$dist_dir" >/dev/null 2>&1; then
    echo "release artifact verification accepted an unrecognized asset" >&2
    exit 1
fi
rm -f "${dist_dir}/unexpected.txt"

# A mutated archive must never survive the checksum gate.
install -m 0644 /bin/false "${dist_dir}/astra-v0.1.0-linux-amd64.tar.gz"
if "${repo_root}/scripts/verify-release-artifacts.sh" 0.1.0 "$dist_dir" >/dev/null 2>&1; then
    echo "release artifact verification accepted a mutated archive" >&2
    exit 1
fi

installer_output="$("${repo_root}/scripts/install-astra.sh" --version 0.1.0 --dry-run)"
grep -Fq "github.com/matrixorigin/Astra/releases/download/v0.1.0" <<< "$installer_output"
if grep -Fqi "astra-suite" <<< "$installer_output"; then
    echo "installer still resolves assets through astra-suite" >&2
    exit 1
fi
