#!/usr/bin/env python3
"""Offline monotonic release promotion and workflow boundary regressions."""

import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import patch

from test_release_build_shells import workflow_run_script


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "release_promotion", ROOT / "scripts/resolve_release_promotion.py"
)
promotion = importlib.util.module_from_spec(spec)
spec.loader.exec_module(promotion)


def release(version, **flags):
    return {"tag_name": f"v{version}", "draft": False, "prerelease": False, **flags}


class ReleasePromotionTests(unittest.TestCase):
    def test_version_order_and_recovery(self):
        cases = (
            ("0.2.3", [[]], True, True),
            ("0.2.3", [[release("0.2.3")]], True, True),
            ("0.2.3", [[release("0.2.4")]], False, False),
            ("0.2.9", [[release("0.3.0"), release("0.2.8")]], False, True),
            ("0.2.9", [[release("0.3.0")], [release("0.2.10")]], False, False),
            ("0.2.10", [[release("0.2.9")]], True, True),
            ("0.2.3", [[release("0.2.4", draft=True),
                         release("0.3.0-rc.1", prerelease=True)]], True, True),
        )
        for version, pages, latest, minor in cases:
            with self.subTest(version=version, pages=pages):
                self.assertEqual(promotion.resolve(version, pages), {
                    "publish_latest": latest, "publish_minor": minor,
                })

    def test_malformed_responses_fail_closed(self):
        for pages in (None, [], {}, [{}], [[{}]],
                      [[release("0.2.3", draft="false")]], [[release(None, tag_name=None)]]):
            with self.subTest(pages=pages), self.assertRaises(ValueError):
                promotion.resolve("0.2.3", pages)

    def test_unknown_tags_freeze_both_aliases_without_blocking_publication(self):
        for tag in ("unknown", "01.2.3", "0.3.0-rc.1", "unexpected\n::error::text"):
            with self.subTest(tag=tag), redirect_stderr(io.StringIO()) as err:
                self.assertEqual(promotion.resolve("0.2.3", [[release("0.2.2")], [release(tag)]]),
                                 {"publish_latest": False, "publish_minor": False})
                self.assertIn("preserving GitHub latest", err.getvalue())
                self.assertEqual(len(err.getvalue().splitlines()), 1)

    def test_unknown_inventory_tag_does_not_mask_invalid_candidate(self):
        with self.assertRaises(ValueError):
            promotion.resolve("0.2.3-rc.1", [[release("unknown")]])

    def test_query_fetches_all_pages_and_never_emits_partial_decisions(self):
        outcomes = (
            subprocess.CompletedProcess([], 0, json.dumps([[release("0.2.4")]])),
            subprocess.CompletedProcess([], 0, json.dumps([[release("unknown")]])),
            subprocess.CompletedProcess([], 0, '{"message":"API error"}'),
            subprocess.CompletedProcess([], 0, '[[{"tag_name":'),
            subprocess.CalledProcessError(1, "gh"),
            subprocess.TimeoutExpired("gh", 120),
        )
        for index, outcome in enumerate(outcomes):
            with self.subTest(outcome=outcome):
                out, err = io.StringIO(), io.StringIO()
                with patch("sys.argv", ["resolve", "matrixorigin/Astra", "0.2.3"]), \
                     patch.object(promotion.subprocess, "run") as run, \
                     redirect_stdout(out), redirect_stderr(err):
                    if isinstance(outcome, Exception):
                        run.side_effect = outcome
                    else:
                        run.return_value = outcome
                    result = promotion.main()
                self.assertEqual(result, 0 if index < 2 else 1)
                self.assertEqual(out.getvalue(),
                                 "publish_latest=false\npublish_minor=false\n" if index < 2 else "")
                command = run.call_args.args[0]
                self.assertIn("--paginate", command)
                self.assertIn("--slurp", command)

    def test_actual_rolling_step_only_writes_admitted_aliases(self):
        script = workflow_run_script(".github/workflows/release.yml", "Promote stable rolling Docker tags")
        stub = r'''
docker() {
    if [ "$3" = create ]; then
        printf '%s\n' "$*" >&2
    elif [ "$3" = inspect ]; then
        printf '%s' 'verified-version-manifest'
    else
        return 99
    fi
}
'''
        for latest, minor, aliases in (
            ("true", "true", ["0.2", "latest"]),
            ("false", "true", ["0.2"]),
            ("false", "false", []),
        ):
            with self.subTest(latest=latest, minor=minor):
                # All provider/registry commands are shell stubs, never real writes.
                result = subprocess.run(["bash", "-c", stub + script], env={
                    **os.environ, "PUBLISH_LATEST": latest, "PUBLISH_MINOR": minor,
                    "IMAGE_NAME": "example/astra", "IMAGE_VERSION": "0.2.9", "ROLLING_VERSION": "0.2",
                }, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = ("buildx imagetools create " +
                            " ".join(f"--tag example/astra:{alias}" for alias in aliases) +
                            " example/astra:0.2.9\n") if aliases else ""
                self.assertEqual(result.stderr, expected)

    def test_workflow_guards_github_latest_and_skips_prerelease_promotion(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        resolver = workflow.split("      - name: Resolve monotonic stable release promotion", 1)[1].split("      - name:", 1)[0]
        self.assertIn("needs.preflight.outputs.prerelease == 'false'", resolver)
        self.assertIn("GH_TOKEN: ${{ github.token }}", resolver)
        publication = workflow.split("      - name: Publish GitHub Release", 1)[1].split("      - name:", 1)[0]
        self.assertIn("PUBLISH_LATEST: ${{ steps.promotion.outputs.publish_latest || 'false' }}", publication)
        self.assertNotIn("needs.preflight.outputs.publish_latest", workflow)


if __name__ == "__main__":
    unittest.main()
