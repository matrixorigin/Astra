#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <version>" >&2
    exit 2
fi

release_version="${1#v}"
if [[ ! "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "invalid release version: $1" >&2
    exit 2
fi

release_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_version="$(
    python3 - "$release_root/Cargo.toml" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as manifest:
    document = tomllib.load(manifest)

print(document["workspace"]["package"]["version"])
PY
)"

if [[ "$release_version" != "$workspace_version" ]]; then
    echo "release version $release_version does not match workspace version $workspace_version" >&2
    exit 1
fi

echo "release version $release_version matches Cargo.toml"
