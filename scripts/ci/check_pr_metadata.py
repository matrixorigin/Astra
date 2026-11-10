#!/usr/bin/env python3
"""Validate a proposed PR title and body with the repository's CI workflow."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from test_release_build_shells import ROOT, workflow_run_script


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--title", required=True, help="exact proposed PR title")
    parser.add_argument("--body-file", required=True, type=Path, help="exact proposed PR body")
    args = parser.parse_args()

    workflow = ".github/workflows/pr-title.yml"
    text = (ROOT / workflow).read_text(encoding="utf-8")
    pattern = re.search(r"(?m)^[ \t]*PATTERN: '([^']*)'[ \t]*$", text)
    if pattern is None:
        parser.error("cannot find the title pattern in the PR metadata workflow")
    with args.body_file.open(encoding="utf-8", newline="") as body_file:
        body = body_file.read()
    with tempfile.TemporaryDirectory() as directory:
        event = Path(directory) / "event.json"
        event.write_text(json.dumps({"pull_request": {"body": body}}), encoding="utf-8")
        env = {
            **os.environ,
            "TITLE": args.title,
            "PATTERN": pattern.group(1),
            "GITHUB_EVENT_PATH": str(event),
        }
        for step in ("Validate title", "Validate capability assessment"):
            script = workflow_run_script(workflow, step)
            result = subprocess.run(["bash", "-eu", "-c", script], env=env, check=False)
            if result.returncode:
                return result.returncode
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
