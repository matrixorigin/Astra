#!/usr/bin/env python3
"""Verify that a staged GitHub Release contains exactly the local assets."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import stat
import subprocess
import sys


SHA256_DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")


def local_assets(artifact_dir: Path) -> dict[str, tuple[int, str]]:
    if not artifact_dir.is_dir():
        raise ValueError(f"release artifact directory does not exist: {artifact_dir}")

    assets: dict[str, tuple[int, str]] = {}
    for asset in sorted(artifact_dir.iterdir(), key=lambda candidate: candidate.name):
        metadata = asset.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise ValueError(f"local release asset must be a regular file: {asset.name}")
        digest = hashlib.sha256()
        with asset.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        assets[asset.name] = (metadata.st_size, f"sha256:{digest.hexdigest()}")

    if not assets:
        raise ValueError("local release asset set is empty")
    return assets


def remote_assets(repository: str, release_id: int) -> dict[str, tuple[int, str]]:
    assets: dict[str, tuple[int, str]] = {}
    page = 1
    while True:
        endpoint = (
            f"repos/{repository}/releases/{release_id}/assets?per_page=100&page={page}"
        )
        result = subprocess.run(
            ["gh", "api", endpoint],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or "unknown error"
            raise RuntimeError(f"could not list staged GitHub Release assets: {detail}")
        try:
            response = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("GitHub returned invalid release asset JSON") from error
        if not isinstance(response, list):
            raise RuntimeError("GitHub returned a non-list release asset response")

        for asset in response:
            if not isinstance(asset, dict):
                raise RuntimeError("GitHub returned an invalid release asset record")
            name = asset.get("name")
            size = asset.get("size")
            digest = asset.get("digest")
            state = asset.get("state")
            if (
                not isinstance(name, str)
                or not name
                or type(size) is not int
                or size < 0
                or not isinstance(digest, str)
                or not SHA256_DIGEST.fullmatch(digest)
                or state != "uploaded"
            ):
                raise RuntimeError(f"GitHub returned an incomplete release asset: {name!r}")
            if name in assets:
                raise RuntimeError(f"GitHub returned duplicate release asset: {name}")
            assets[name] = (size, digest)

        if len(response) < 100:
            return assets
        page += 1


def verify_assets(
    expected: dict[str, tuple[int, str]], actual: dict[str, tuple[int, str]]
) -> None:
    expected_names = set(expected)
    actual_names = set(actual)
    missing = sorted(expected_names - actual_names)
    extra = sorted(actual_names - expected_names)
    mismatched = sorted(
        name
        for name in expected_names & actual_names
        if expected[name] != actual[name]
    )
    if not missing and not extra and not mismatched:
        return

    details = []
    if missing:
        details.append(f"missing={','.join(missing)}")
    if extra:
        details.append(f"extra={','.join(extra)}")
    if mismatched:
        details.append(f"size-or-digest-mismatch={','.join(mismatched)}")
    raise ValueError("staged GitHub Release asset mismatch: " + "; ".join(details))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository")
    parser.add_argument("release_id", type=int)
    parser.add_argument("artifact_dir", type=Path)
    arguments = parser.parse_args()

    if not re.fullmatch(r"[^/]+/[^/]+", arguments.repository):
        parser.error("repository must be OWNER/REPO")
    if arguments.release_id <= 0:
        parser.error("release_id must be positive")

    try:
        expected = local_assets(arguments.artifact_dir)
        actual = remote_assets(arguments.repository, arguments.release_id)
        verify_assets(expected, actual)
    except (OSError, RuntimeError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
