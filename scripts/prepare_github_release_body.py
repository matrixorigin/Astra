#!/usr/bin/env python3
"""Prepare and verify one owner-bound, idempotent GitHub Release body."""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path


OWNER_PREFIX = "<!-- astra-release-owner:v1 "
SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
RUN_ID_PATTERN = re.compile(r"[1-9][0-9]*")


class ReleaseBodyError(ValueError):
    """The existing draft cannot be proven to belong to this release."""


@dataclass(frozen=True)
class PreparedBody:
    body: str
    generate_release_notes: bool


def ownership_marker(owner_run_id: str, source_sha: str, source_tag: str) -> str:
    if RUN_ID_PATTERN.fullmatch(owner_run_id) is None:
        raise ReleaseBodyError("owner run ID must be a positive integer")
    if SHA_PATTERN.fullmatch(source_sha) is None:
        raise ReleaseBodyError("source SHA must be 40 lowercase hexadecimal characters")
    if not source_tag or any(character.isspace() for character in source_tag):
        raise ReleaseBodyError("source tag must be one non-empty token")
    return (
        f"{OWNER_PREFIX}run={owner_run_id} sha={source_sha} "
        f"tag={source_tag} -->"
    )


def _release_body(release: dict, expected_tag: str) -> str:
    if release.get("draft") is not True:
        raise ReleaseBodyError("the existing GitHub Release is not a draft")
    if release.get("tag_name") != expected_tag:
        raise ReleaseBodyError("the existing draft targets a different tag")
    body = release.get("body")
    if not isinstance(body, str):
        raise ReleaseBodyError("the existing draft has no textual body")
    return body


def prepare_body(
    owner_run_id: str,
    source_sha: str,
    source_tag: str,
    existing_release: dict | None,
) -> PreparedBody:
    marker = ownership_marker(owner_run_id, source_sha, source_tag)
    if existing_release is None:
        return PreparedBody(body=f"{marker}\n", generate_release_notes=True)

    body = _release_body(existing_release, source_tag)
    if not body.startswith(f"{marker}\n") or body.count(OWNER_PREFIX) != 1:
        raise ReleaseBodyError(
            "the existing draft body is not owned by the immutable release run"
        )
    return PreparedBody(body=body, generate_release_notes=False)


def verify_body(
    owner_run_id: str,
    source_sha: str,
    source_tag: str,
    release: dict,
    expected_body: str,
    generated_notes: bool,
) -> None:
    marker = ownership_marker(owner_run_id, source_sha, source_tag)
    actual_body = _release_body(release, source_tag)
    if not actual_body.startswith(f"{marker}\n") or actual_body.count(OWNER_PREFIX) != 1:
        raise ReleaseBodyError(
            "the staged draft lost or duplicated its release-owner marker"
        )
    if not generated_notes and actual_body != expected_body:
        raise ReleaseBodyError(
            "the staged draft body changed while reusing canonical release notes"
        )


def _load_json(path: Path) -> dict:
    document = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(document, dict):
        raise ReleaseBodyError(f"expected one JSON object in {path}")
    return document


def _common_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--owner-run-id", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--source-tag", required=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    prepare = subparsers.add_parser("prepare")
    _common_arguments(prepare)
    prepare.add_argument("--existing-release-json", type=Path)
    prepare.add_argument("--output", required=True, type=Path)

    verify = subparsers.add_parser("verify")
    _common_arguments(verify)
    verify.add_argument("--release-json", required=True, type=Path)
    verify.add_argument("--expected-body", required=True, type=Path)
    verify.add_argument(
        "--generated-notes", required=True, choices=("true", "false")
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "prepare":
        existing = (
            _load_json(args.existing_release_json)
            if args.existing_release_json is not None
            else None
        )
        prepared = prepare_body(
            args.owner_run_id, args.source_sha, args.source_tag, existing
        )
        args.output.write_text(prepared.body, encoding="utf-8")
        print("true" if prepared.generate_release_notes else "false")
        return 0

    verify_body(
        args.owner_run_id,
        args.source_sha,
        args.source_tag,
        _load_json(args.release_json),
        args.expected_body.read_text(encoding="utf-8"),
        args.generated_notes == "true",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
