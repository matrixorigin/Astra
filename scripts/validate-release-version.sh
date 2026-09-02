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
python3 - "$release_root" "$release_version" <<'PY'
import json
import re
import sys
import tomllib
from pathlib import Path

root = Path(sys.argv[1])
expected = sys.argv[2]

with (root / "Cargo.toml").open("rb") as manifest:
    document = tomllib.load(manifest)

versions = {
    "Cargo workspace": document["workspace"]["package"]["version"],
}

with (root / "packages/sdk/package.json").open(encoding="utf-8") as package:
    versions["TypeScript SDK"] = json.load(package)["version"]

def yaml_scalar(path: Path, key: str) -> str:
    pattern = re.compile(rf"^{re.escape(key)}:\s*[\"']?([^\"'#\s]+)")
    for line in path.read_text(encoding="utf-8").splitlines():
        if match := pattern.match(line):
            return match.group(1)
    raise SystemExit(f"{path}: missing {key}")

versions["CITATION.cff"] = yaml_scalar(root / "CITATION.cff", "version")
versions["Helm chart"] = yaml_scalar(
    root / "deployment/kubernetes/chart/Chart.yaml", "version"
)
versions["Helm appVersion"] = yaml_scalar(
    root / "deployment/kubernetes/chart/Chart.yaml", "appVersion"
)

mismatches = {name: value for name, value in versions.items() if value != expected}
if mismatches:
    for name, value in mismatches.items():
        print(
            f"release version {expected} does not match {name} version {value}",
            file=sys.stderr,
        )
    raise SystemExit(1)

print(f"release version {expected} matches " + ", ".join(versions))
PY
