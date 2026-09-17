#!/usr/bin/env python3
"""Synchronize Astra's versioned release surfaces without publishing anything."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
from typing import NoReturn


ROOT = Path(__file__).resolve().parent.parent
VERSION_FILES = (
    Path("Cargo.toml"),
    Path("Cargo.lock"),
    Path("packages/sdk/package.json"),
    Path("packages/sdk/package-lock.json"),
    Path("web/package.json"),
    Path("web/package-lock.json"),
    Path("CITATION.cff"),
    Path("deployment/kubernetes/chart/Chart.yaml"),
    Path("deployment/all-in-one/.env.example"),
    Path(".env.production.example"),
)
SEMVER = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-(?:[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)


def fail(message: str) -> NoReturn:
    raise SystemExit(f"error: {message}")


def normalize_version(argument: str) -> str:
    version = argument.removeprefix("v")
    if SEMVER.fullmatch(version) is None:
        fail(f"invalid semantic version: {argument}")
    prerelease = version.partition("-")[2]
    if any(
        identifier.isdigit() and len(identifier) > 1 and identifier.startswith("0")
        for identifier in prerelease.split(".")
        if identifier
    ):
        fail(f"numeric prerelease identifiers cannot have leading zeros: {version}")
    return version


def replace_once(text: str, pattern: str, replacement: str, label: str) -> str:
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count != 1:
        fail(f"could not locate exactly one {label}")
    return updated


def update_workspace_manifest(text: str, version: str) -> str:
    section_match = re.search(
        r"^\[workspace\.package\]\n(?P<body>.*?)(?=^\[|\Z)",
        text,
        flags=re.MULTILINE | re.DOTALL,
    )
    if section_match is None:
        fail("Cargo.toml is missing [workspace.package]")
    body = replace_once(
        section_match.group("body"),
        r'^(version\s*=\s*")[^"]+("\s*)$',
        rf"\g<1>{version}\g<2>",
        "Cargo workspace version",
    )
    return text[: section_match.start("body")] + body + text[section_match.end("body") :]


def update_lockfile(text: str, version: str) -> str:
    package_pattern = re.compile(
        r"^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)",
        flags=re.MULTILINE | re.DOTALL,
    )
    changed = 0

    def update_package(match: re.Match[str]) -> str:
        nonlocal changed
        block = match.group(0)
        name_match = re.search(r'^name = "([^"]+)"$', block, flags=re.MULTILINE)
        if (
            name_match is None
            or not name_match.group(1).startswith("astra-")
            or re.search(r"^source = ", block, flags=re.MULTILINE)
        ):
            return block
        updated = replace_once(
            block,
            r'^(version = ")[^"]+("\s*)$',
            rf"\g<1>{version}\g<2>",
            f"Cargo.lock version for {name_match.group(1)}",
        )
        changed += 1
        return updated

    updated = package_pattern.sub(update_package, text)
    if changed == 0:
        fail("Cargo.lock contains no local astra-* packages")
    return updated


def update_package_json(text: str, version: str, lockfile: bool) -> str:
    document = json.loads(text)
    document["version"] = version
    if lockfile:
        root_package = document.get("packages", {}).get("")
        if not isinstance(root_package, dict):
            fail("npm lockfile is missing packages['']")
        root_package["version"] = version
        for package_path, package in document["packages"].items():
            if package_path and package.get("name") == "@astra/sdk":
                package["version"] = version
    return json.dumps(document, ensure_ascii=False, indent=2) + "\n"


def render_updates(version: str) -> dict[Path, str]:
    source = {
        path: (ROOT / path).read_text(encoding="utf-8") for path in VERSION_FILES
    }
    updates = {
        Path("Cargo.toml"): update_workspace_manifest(source[Path("Cargo.toml")], version),
        Path("Cargo.lock"): update_lockfile(source[Path("Cargo.lock")], version),
        Path("packages/sdk/package.json"): update_package_json(
            source[Path("packages/sdk/package.json")], version, False
        ),
        Path("packages/sdk/package-lock.json"): update_package_json(
            source[Path("packages/sdk/package-lock.json")], version, True
        ),
        Path("web/package.json"): update_package_json(
            source[Path("web/package.json")], version, False
        ),
        Path("web/package-lock.json"): update_package_json(
            source[Path("web/package-lock.json")], version, True
        ),
    }
    updates[Path("CITATION.cff")] = replace_once(
        source[Path("CITATION.cff")],
        r"^version:\s*.*$",
        f"version: {version}",
        "CITATION.cff version",
    )
    chart = source[Path("deployment/kubernetes/chart/Chart.yaml")]
    chart = replace_once(chart, r"^version:\s*.*$", f"version: {version}", "Helm version")
    updates[Path("deployment/kubernetes/chart/Chart.yaml")] = replace_once(
        chart,
        r"^appVersion:\s*.*$",
        f'appVersion: "{version}"',
        "Helm appVersion",
    )
    updates[Path("deployment/all-in-one/.env.example")] = replace_once(
        source[Path("deployment/all-in-one/.env.example")],
        r"^ASTRA_IMAGE=.*$",
        f"ASTRA_IMAGE=matrixorigin/astra:{version}",
        "all-in-one Astra image",
    )
    updates[Path(".env.production.example")] = replace_once(
        source[Path(".env.production.example")],
        r"^ASTRA_IMAGE=.*$",
        f"ASTRA_IMAGE=matrixorigin/astra:{version}",
        "production Astra image",
    )
    return updates


def write_atomically(updates: dict[Path, str]) -> None:
    staged: list[tuple[Path, Path]] = []
    try:
        for relative, content in updates.items():
            destination = ROOT / relative
            descriptor, temporary_name = tempfile.mkstemp(
                prefix=f".{destination.name}.release-", dir=destination.parent
            )
            temporary = Path(temporary_name)
            with os.fdopen(descriptor, "w", encoding="utf-8") as output:
                output.write(content)
            os.chmod(temporary, stat.S_IMODE(destination.stat().st_mode))
            staged.append((temporary, destination))
        for temporary, destination in staged:
            os.replace(temporary, destination)
    finally:
        for temporary, _ in staged:
            temporary.unlink(missing_ok=True)


def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: prepare-release-version.py <version>")
    version = normalize_version(sys.argv[1])

    dirty = subprocess.run(
        ["git", "status", "--porcelain=v1", "--", *(str(path) for path in VERSION_FILES)],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if dirty:
        fail("release version files already have uncommitted changes; commit or restore them first")

    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    current_match = re.search(
        r'^\[workspace\.package\]\n.*?^version\s*=\s*"([^"]+)"',
        manifest,
        flags=re.MULTILINE | re.DOTALL,
    )
    if current_match is None:
        fail("could not read the current Cargo workspace version")
    current = current_match.group(1)
    subprocess.run(
        [str(ROOT / "scripts/validate-release-version.sh"), current],
        cwd=ROOT,
        check=True,
    )

    updates = render_updates(version)
    if all(
        (ROOT / path).read_text(encoding="utf-8") == content
        for path, content in updates.items()
    ):
        print(f"release version {version} is already synchronized")
        return
    write_atomically(updates)
    subprocess.run(
        [str(ROOT / "scripts/validate-release-version.sh"), version],
        cwd=ROOT,
        check=True,
    )
    print(f"prepared Astra {version} release metadata in {len(updates)} files")
    print("review the diff and the pinned MatrixOne/Memoria compatibility digests before committing")


if __name__ == "__main__":
    main()
