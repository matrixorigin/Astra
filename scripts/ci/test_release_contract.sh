#!/usr/bin/env bash
# Exercise the release metadata, archive-set, and installer contracts locally.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-release-contract.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT HUP INT TERM

package_dir="${fixture_root}/package"
dist_dir="${fixture_root}/dist"
mkdir -p "$package_dir" "$dist_dir"
install -m 0755 /bin/true "${package_dir}/astra"
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
    tar -czf "${dist_dir}/${archive}" -C "$package_dir" astra LICENSE
    printf '%s  %s\n' "$(sha256_file "${dist_dir}/${archive}")" "$archive" \
        > "${dist_dir}/${archive}.sha256"
done

"${repo_root}/scripts/verify-release-artifacts.sh" 0.1.0 "$dist_dir"
expected_manifest_hash="$(awk '{ print $1 }' "${dist_dir}/checksums.txt.sha256")"
actual_manifest_hash="$(sha256_file "${dist_dir}/checksums.txt")"
[[ "$expected_manifest_hash" = "$actual_manifest_hash" ]]

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
