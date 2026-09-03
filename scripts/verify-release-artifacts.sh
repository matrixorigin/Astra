#!/usr/bin/env bash
# Validate the complete CLI release set and write aggregate checksum files.

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <version> <artifact-directory>" >&2
    exit 2
fi

release_version="${1#v}"
artifact_dir="$2"
if [[ ! "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "invalid release version: $1" >&2
    exit 2
fi
if [[ ! -d "$artifact_dir" ]]; then
    echo "artifact directory does not exist: $artifact_dir" >&2
    exit 2
fi

cd "$artifact_dir"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "sha256sum or shasum is required" >&2
        return 1
    fi
}

expected=(
    "linux-amd64"
    "linux-arm64"
    "darwin-amd64"
    "darwin-arm64"
)

shopt -s nullglob
archives=(astra-v*.tar.gz)
checksum_files=(astra-v*.tar.gz.sha256)
if [[ "${#archives[@]}" -ne "${#expected[@]}" || "${#checksum_files[@]}" -ne "${#expected[@]}" ]]; then
    echo "expected ${#expected[@]} archives and checksums; found ${#archives[@]} and ${#checksum_files[@]}" >&2
    exit 1
fi

for suffix in "${expected[@]}"; do
    archive="astra-v${release_version}-${suffix}.tar.gz"
    [[ -f "$archive" ]] || { echo "missing release archive: $archive" >&2; exit 1; }
    [[ -f "${archive}.sha256" ]] || { echo "missing release checksum: ${archive}.sha256" >&2; exit 1; }
    read -r published_hash published_name extra < "${archive}.sha256"
    if [[ ! "$published_hash" =~ ^[0-9a-f]{64}$ || "$published_name" != "$archive" || -n "${extra:-}" ]]; then
        echo "invalid checksum record: ${archive}.sha256" >&2
        exit 1
    fi
    actual_hash="$(sha256_file "$archive")"
    if [[ "$published_hash" != "$actual_hash" ]]; then
        echo "checksum mismatch for $archive" >&2
        exit 1
    fi

    members="$(tar -tzf "$archive" | sort)"
    if [[ "$members" != $'LICENSE\nastra' ]]; then
        echo "$archive must contain exactly astra and LICENSE at its root" >&2
        printf '%s\n' "$members" >&2
        exit 1
    fi
done

sort -k2 "${checksum_files[@]}" > checksums.txt
printf '%s  %s\n' "$(sha256_file checksums.txt)" checksums.txt > checksums.txt.sha256
