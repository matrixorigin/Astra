#!/usr/bin/env python3
"""Classify changed paths into Astra CI execution scopes.

The classifier is deliberately conservative: an unknown diff or a change to CI
itself enables every scope. Documentation and repository-governance changes are
handled by the always-on lightweight repository checks.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


SCOPES = (
    "rust",
    "sdk",
    "web",
    "harness",
    "test_cli",
    "test_runtime",
    "test_services",
    "test_core",
    "online_core",
    "online_integration",
)
RUST_SCOPES = (
    "rust",
    "test_cli",
    "test_runtime",
    "test_services",
    "test_core",
    "online_core",
    "online_integration",
)
SERVICE_CRATES = {"services", "astra-turn-core", "astra-plan", "astra-prompts"}
LIGHTWEIGHT_ROOT_FILES = {
    ".dockerignore",
    ".editorconfig",
    ".gitattributes",
    ".gitignore",
    "CITATION.cff",
    "CODE_OF_CONDUCT.md",
    "CONTRIBUTING.md",
    "LICENSE",
    "README.md",
    "SUPPORT.md",
}


def _scopes(*enabled: str) -> dict[str, bool]:
    return {scope: scope in enabled for scope in SCOPES}


def _enable(result: dict[str, bool], *scopes: str) -> None:
    for scope in scopes:
        result[scope] = True


def _normalize(path: str) -> str:
    normalized = path.strip().replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized


def _is_lightweight(path: str) -> bool:
    return (
        path in LIGHTWEIGHT_ROOT_FILES
        or path.endswith((".md", ".mdc"))
        or path.startswith(
            (
                ".agent/",
                ".claude/",
                ".cursor/",
                ".github/CODEOWNERS",
                ".github/ISSUE_TEMPLATE/",
                ".github/PULL_REQUEST_TEMPLATE.md",
                ".github/dependabot.yml",
                ".github/mergify.yml",
                ".github/release.yml",
                ".kiro/",
                "deployment/",
                "docs/",
                "monitoring/",
                "plans/",
                "scripts/",
            )
        )
    )


def _rust_scope_for(path: str, result: dict[str, bool]) -> None:
    _enable(result, "rust")
    parts = path.split("/")
    crate = parts[1] if len(parts) > 1 else ""
    is_test_only = "/tests/" in path or path.endswith(("/tests.rs", "/test.rs"))

    if crate == "astra-cli":
        _enable(result, "test_cli")
        return
    if crate == "runtime":
        _enable(result, "test_runtime")
        if is_test_only:
            online_lane = "online_integration" if "/tests/" in path else "online_core"
            _enable(result, online_lane)
        else:
            # Runtime is consumed by the CLI and the bridge-hook tests in shard D.
            _enable(result, "test_cli", "test_core", "online_core", "online_integration")
        return
    if crate in SERVICE_CRATES:
        _enable(result, "test_services")
        if crate == "services":
            _enable(result, "online_core")
        elif crate == "astra-plan":
            _enable(result, "online_integration")
        elif crate == "astra-turn-core" and not is_test_only:
            _enable(result, "online_core")
        if not is_test_only:
            # These crates feed both runtime orchestration and CLI execution.
            _enable(result, "test_runtime", "test_cli")
        return

    _enable(result, "test_core")
    if not is_test_only:
        # Core crates are shared broadly. Keep their downstream behavioral gates
        # conservative; workspace clippy also compiles every downstream target.
        _enable(
            result,
            "test_services",
            "test_runtime",
            "test_cli",
            "online_core",
            "online_integration",
        )


def classify(paths: list[str]) -> dict[str, bool]:
    normalized = {_normalize(path) for path in paths if _normalize(path)}
    if not normalized:
        return _scopes(*SCOPES)

    result = _scopes()
    for path in normalized:
        # Changes to routing or workflow behavior must exercise every route.
        if path.startswith((".github/workflows/", ".github/actions/", "scripts/ci/")) or path == "Makefile":
            return _scopes(*SCOPES)

        if (
            path in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"}
            or path.startswith((".cargo/", ".config/nextest"))
        ):
            _enable(result, *RUST_SCOPES)
            continue
        if path == ".nvmrc":
            _enable(result, "sdk", "web")
            continue
        if path.startswith("packages/sdk/"):
            # The web workspace consumes the local SDK package.
            _enable(result, "sdk", "web")
            continue
        if path.startswith("web/"):
            _enable(result, "web")
            continue
        if path.startswith("scripts/harness/"):
            _enable(result, "harness")
            continue
        if path.startswith("config/") or path in {".env.example", ".env.production.example"}:
            _enable(
                result,
                "rust",
                "test_core",
                "test_runtime",
                "online_core",
                "online_integration",
            )
            continue
        if path == "Dockerfile":
            # make lint verifies its Rust version remains aligned with the toolchain.
            _enable(result, "rust")
            continue
        if path.startswith("crates/"):
            _rust_scope_for(path, result)
            continue
        if _is_lightweight(path):
            continue

        # A new or unclassified area must not silently bypass its relevant gate.
        return _scopes(*SCOPES)

    return result


def changed_paths_from_event() -> list[str]:
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path:
        raise RuntimeError("GITHUB_EVENT_PATH is not set")
    event = json.loads(Path(event_path).read_text(encoding="utf-8"))
    if "pull_request" in event:
        base = event["pull_request"]["base"]["sha"]
        head = event["pull_request"]["head"]["sha"]
    else:
        base = event.get("before")
        head = event.get("after") or os.environ.get("GITHUB_SHA")
    if not base or not head or set(base) == {"0"}:
        return []
    output = subprocess.check_output(
        ["git", "diff", "--name-only", "-z", base, head], stderr=subprocess.STDOUT
    )
    return [item.decode("utf-8") for item in output.split(b"\0") if item]


def emit(scopes: dict[str, bool], paths: list[str], *, fallback: str | None = None) -> None:
    if fallback:
        print(f"::warning::{fallback}; enabling every CI scope", file=sys.stderr)
    for scope in SCOPES:
        print(f"{scope}={'true' if scopes[scope] else 'false'}")
    print("changed_paths=" + (", ".join(sorted(paths)) or "<unknown; full fallback>"))

    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with Path(github_output).open("a", encoding="utf-8") as output:
            for scope in SCOPES:
                output.write(f"{scope}={'true' if scopes[scope] else 'false'}\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="*", help="explicit changed paths; omit in GitHub Actions")
    args = parser.parse_args()
    try:
        paths = args.paths or changed_paths_from_event()
        emit(classify(paths), paths)
    except (
        OSError,
        RuntimeError,
        subprocess.CalledProcessError,
        KeyError,
        TypeError,
        ValueError,
    ) as error:
        # Scope detection is an optimization, never a reason to omit validation.
        emit(_scopes(*SCOPES), [], fallback=f"CI scope detection failed: {error}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
