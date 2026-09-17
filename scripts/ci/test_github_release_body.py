#!/usr/bin/env python3
"""Exercise idempotent, owner-bound GitHub Release draft bodies."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "prepare_github_release_body.py"
SPEC = importlib.util.spec_from_file_location("prepare_github_release_body", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release_body = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release_body
SPEC.loader.exec_module(release_body)

OWNER_RUN_ID = "12345"
SOURCE_SHA = "a" * 40
SOURCE_TAG = "v0.3.0"


def draft(body: str) -> dict:
    return {"id": 77, "draft": True, "tag_name": SOURCE_TAG, "body": body}


def main() -> None:
    first = release_body.prepare_body(OWNER_RUN_ID, SOURCE_SHA, SOURCE_TAG, None)
    assert first.generate_release_notes is True

    generated_body = first.body + "\n## What's Changed\n\n* One canonical note.\n"
    release_body.verify_body(
        OWNER_RUN_ID,
        SOURCE_SHA,
        SOURCE_TAG,
        draft(generated_body),
        first.body,
        generated_notes=True,
    )

    # Stage the same owned draft twice. Neither pass regenerates nor appends
    # notes, and both preserve the byte-identical canonical body.
    second = release_body.prepare_body(
        OWNER_RUN_ID, SOURCE_SHA, SOURCE_TAG, draft(generated_body)
    )
    assert second.generate_release_notes is False
    assert second.body == generated_body
    release_body.verify_body(
        OWNER_RUN_ID,
        SOURCE_SHA,
        SOURCE_TAG,
        draft(second.body),
        second.body,
        generated_notes=False,
    )
    third = release_body.prepare_body(
        OWNER_RUN_ID, SOURCE_SHA, SOURCE_TAG, draft(second.body)
    )
    assert third.generate_release_notes is False
    assert third.body == second.body

    for unrelated_body in (
        "Manual draft notes\n",
        first.body + first.body,
        first.body.replace("run=12345", "run=99999"),
    ):
        try:
            release_body.prepare_body(
                OWNER_RUN_ID, SOURCE_SHA, SOURCE_TAG, draft(unrelated_body)
            )
        except release_body.ReleaseBodyError:
            pass
        else:
            raise AssertionError("an unrelated or ambiguous draft body was accepted")


if __name__ == "__main__":
    main()
