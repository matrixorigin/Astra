#!/usr/bin/env python3
"""Resolve monotonic rolling-tag promotion from published GitHub releases.

The unified workflow serializes publication and publishes GitHub assets before
rolling Docker tags. Published stable releases are therefore the high-water
marks for that workflow, including when an older release is recovered. Manual
registry writes are outside this ownership contract.
"""

import argparse
import json
import re
import subprocess
import sys


STABLE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def stable_version(version):
    match = STABLE.fullmatch(version.removeprefix("v"))
    if match is None:
        raise ValueError(f"Unrecognized stable release version: {version!r}")
    return tuple(map(int, match.groups()))


def resolve(version, pages):
    candidate = stable_version(version)
    if not isinstance(pages, list) or not pages:
        raise ValueError("Expected paginated GitHub release arrays")
    published = []
    unknown_tags = []
    for page in pages:
        if not isinstance(page, list):
            raise ValueError("Expected a GitHub release array on every page")
        for release in page:
            if (not isinstance(release, dict)
                    or type(release.get("draft")) is not bool
                    or type(release.get("prerelease")) is not bool):
                raise ValueError("Malformed GitHub release record")
            if release["draft"] or release["prerelease"]:
                continue
            tag = release.get("tag_name")
            if not isinstance(tag, str):
                raise ValueError("Published release is missing its tag")
            try:
                published.append(stable_version(tag))
            except ValueError:
                unknown_tags.append(tag)
    if unknown_tags:
        # Inventory transport/schema failures remain fatal, but an unfamiliar
        # tag must not prevent publication of verified immutable artifacts. Its
        # release line cannot safely be inferred: freeze BOTH mutable aliases.
        print("warning: Unrecognized published stable tags; preserving GitHub latest "
              "and all rolling Docker aliases: " + json.dumps(unknown_tags), file=sys.stderr)
        return {"publish_latest": False, "publish_minor": False}
    return {
        "publish_latest": not any(item > candidate for item in published),
        "publish_minor": not any(
            item[:2] == candidate[:2] and item > candidate for item in published
        ),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository")
    parser.add_argument("version", help="stable version already validated by release preflight")
    args = parser.parse_args()
    try:
        stable_version(args.version)
        response = subprocess.run(
            ["gh", "api", "--paginate", "--slurp",
             f"repos/{args.repository}/releases?per_page=100"],
            check=True, capture_output=True, text=True, timeout=120,
        )
        decision = resolve(args.version, json.loads(response.stdout))
    except (OSError, subprocess.SubprocessError, ValueError) as error:
        print(f"Cannot safely determine release promotion: {error}", file=sys.stderr)
        return 1
    # Emit nothing until every page has been fetched and validated successfully.
    for key, value in decision.items():
        print(f"{key}={str(value).lower()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
