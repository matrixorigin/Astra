#!/usr/bin/env bash
set -euo pipefail

syntax_only=false
if [[ $# -eq 2 && "$2" == "--syntax-only" ]]; then
    syntax_only=true
elif [[ $# -ne 1 ]]; then
    echo "usage: $0 <version> [--syntax-only]" >&2
    exit 2
fi

release_version="${1#v}"
if [[ ! "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "invalid release version: $1" >&2
    exit 2
fi

release_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python3 - "$release_root" "$release_version" "$syntax_only" <<'PY'
import json
import re
import sys
import tomllib
from pathlib import Path

root = Path(sys.argv[1])
expected = sys.argv[2]
syntax_only = sys.argv[3] == "true"

core, separator, prerelease = expected.partition("-")
core_identifiers = core.split(".")
if len(core_identifiers) != 3 or any(
    not identifier.isdigit()
    or (len(identifier) > 1 and identifier.startswith("0"))
    for identifier in core_identifiers
):
    raise SystemExit(f"invalid semantic release version: {expected}")
if separator:
    prerelease_identifiers = prerelease.split(".")
    if any(
        not identifier
        or re.fullmatch(r"[0-9A-Za-z-]+", identifier) is None
        or (identifier.isdigit() and len(identifier) > 1 and identifier.startswith("0"))
        for identifier in prerelease_identifiers
    ):
        raise SystemExit(f"invalid semantic prerelease version: {expected}")

if syntax_only:
    print(f"release version syntax is valid: {expected}")
    raise SystemExit(0)

with (root / "Cargo.toml").open("rb") as manifest:
    document = tomllib.load(manifest)

versions = {
    "Cargo workspace": document["workspace"]["package"]["version"],
}

with (root / "packages/sdk/package.json").open(encoding="utf-8") as package:
    versions["TypeScript SDK"] = json.load(package)["version"]

with (root / "web/package.json").open(encoding="utf-8") as package:
    versions["Web application"] = json.load(package)["version"]

with (root / "packages/sdk/package-lock.json").open(encoding="utf-8") as package:
    versions["TypeScript SDK lockfile"] = json.load(package)["version"]

with (root / "web/package-lock.json").open(encoding="utf-8") as package:
    web_lock = json.load(package)
    versions["Web application lockfile"] = web_lock["version"]
    local_sdk_versions = {
        entry["version"]
        for entry in web_lock["packages"].values()
        if entry.get("name") == "@astra/sdk" and "version" in entry
    }
    if len(local_sdk_versions) != 1:
        raise SystemExit(
            "web/package-lock.json: expected one version for the linked @astra/sdk package"
        )
    versions["Web lockfile linked SDK"] = local_sdk_versions.pop()

with (root / "Cargo.lock").open("rb") as lockfile:
    locked_packages = tomllib.load(lockfile)["package"]
for package in locked_packages:
    if package["name"].startswith("astra-") and "source" not in package:
        versions[f"Cargo.lock {package['name']}"] = package["version"]

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

def env_value(path: Path, key: str) -> str:
    pattern = re.compile(rf"^{re.escape(key)}=(.*)$")
    for line in path.read_text(encoding="utf-8").splitlines():
        if match := pattern.match(line):
            return match.group(1).strip()
    raise SystemExit(f"{path}: missing {key}")

stack_env = root / "deployment/all-in-one/.env.example"
versions["All-in-one Astra image"] = env_value(stack_env, "ASTRA_IMAGE").removeprefix(
    "matrixorigin/astra:"
)
versions["Production Astra image"] = env_value(
    root / ".env.production.example", "ASTRA_IMAGE"
).removeprefix("matrixorigin/astra:")

for key, repository in (
    ("MEMORIA_IMAGE", "matrixorigin/memoria"),
    ("MATRIXONE_IMAGE", "matrixorigin/matrixone"),
):
    value = env_value(stack_env, key)
    if re.fullmatch(rf"{re.escape(repository)}@sha256:[0-9a-f]{{64}}", value) is None:
        raise SystemExit(
            f"{stack_env}: {key} must pin {repository} by a full sha256 manifest digest"
        )

mismatches = {name: value for name, value in versions.items() if value != expected}
if mismatches:
    for name, value in sorted(mismatches.items()):
        print(
            f"release version {expected} does not match {name} version {value}",
            file=sys.stderr,
        )
    raise SystemExit(1)

print(f"release version {expected} matches " + ", ".join(versions))
PY
