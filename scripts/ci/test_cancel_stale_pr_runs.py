#!/usr/bin/env python3

from __future__ import annotations

from contextlib import redirect_stdout
from io import StringIO
from itertools import permutations
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parent))

from cancel_stale_pr_runs import (  # noqa: E402
    ACTIVE_RUN_STATUSES,
    GitHubApi,
    MAX_SUPERSEDED_RUNS,
    RejectRedirects,
    SupersededHead,
    cancel_superseded_runs,
    parse_args,
    select_superseded_runs,
)


OLD_SHA = "a" * 40
CURRENT_SHA = "b" * 40
NEWER_SHA = "c" * 40
LATEST_SHA = "d" * 40


def workflow_run(
    run_id: int,
    *,
    sha: str = OLD_SHA,
    status: str = "in_progress",
    repository_id: int = 101,
    branch: str = "fix/runtime",
) -> dict[str, object]:
    return {
        "id": run_id,
        "name": "Test Suite",
        "status": status,
        "head_sha": sha,
        "head_branch": branch,
        "head_repository": {"id": repository_id, "full_name": "contributor/Astra"},
    }


class FakeApi:
    def __init__(self, runs: list[dict[str, object]]) -> None:
        self.runs = runs
        self.listed_heads: list[str] = []
        self.cancelled: list[int] = []
        self.finished_before_cancel: set[int] = set()

    def list_runs(self, repository: str, head_sha: str) -> list[dict[str, object]]:
        self.listed_heads.append(head_sha)
        return self.runs

    def cancel_run(self, repository: str, run_id: int) -> bool:
        if run_id in self.finished_before_cancel:
            return False
        self.cancelled.append(run_id)
        return True


class SelectSupersededRunsTests(unittest.TestCase):
    def test_selects_only_the_exact_fork_ref_sha_and_active_status(self) -> None:
        head = SupersededHead(101, "fix/runtime", OLD_SHA)
        runs = [
            workflow_run(1),
            workflow_run(2, sha=CURRENT_SHA),
            workflow_run(3, repository_id=202),
            workflow_run(4, branch="fix/other"),
            workflow_run(5, status="completed"),
            {"id": 6, "status": "queued", "head_sha": OLD_SHA},
        ]

        self.assertEqual(
            [run["id"] for run in select_superseded_runs(runs, head)], [1]
        )

    def test_deduplicates_a_run_observed_during_pagination(self) -> None:
        head = SupersededHead(101, "fix/runtime", OLD_SHA)
        run = workflow_run(7)

        self.assertEqual(
            [item["id"] for item in select_superseded_runs([run, run], head)], [7]
        )

    def test_every_controller_order_cancels_old_generations_only(self) -> None:
        runs = [
            workflow_run(1, sha=OLD_SHA),
            workflow_run(2, sha=CURRENT_SHA),
            workflow_run(3, sha=NEWER_SHA),
            workflow_run(4, sha=LATEST_SHA),
        ]

        for controller_order in permutations((OLD_SHA, CURRENT_SHA, NEWER_SHA)):
            with self.subTest(controller_order=controller_order):
                selected = {
                    int(run["id"])
                    for sha in controller_order
                    for run in select_superseded_runs(
                        runs, SupersededHead(101, "fix/runtime", sha)
                    )
                }

                self.assertEqual(selected, {1, 2, 3})


class GitHubApiTests(unittest.TestCase):
    def test_list_runs_filters_by_superseded_sha_and_follows_pagination(self) -> None:
        api = GitHubApi("test-token")
        second_page = "https://api.github.com/repos/matrixorigin/Astra/actions/runs?page=2"
        api._request = Mock(  # type: ignore[method-assign]
            side_effect=[
                (
                    json.dumps({"workflow_runs": [workflow_run(1)]}).encode(),
                    {"Link": f'<{second_page}>; rel="next"'},
                ),
                (json.dumps({"workflow_runs": [workflow_run(2)]}).encode(), {}),
            ]
        )

        self.assertEqual(
            [run["id"] for run in api.list_runs("matrixorigin/Astra", OLD_SHA)],
            [1, 2],
        )
        first_url = api._request.call_args_list[0].args[1]
        self.assertIn(f"head_sha={OLD_SHA}", first_url)
        self.assertIn("event=pull_request", first_url)

    def test_request_rejects_a_foreign_origin_before_sending_the_token(self) -> None:
        api = GitHubApi("test-token")

        with self.assertRaisesRegex(RuntimeError, "another origin"):
            api._request("GET", "https://example.test/steal")
        with self.assertRaisesRegex(RuntimeError, "another origin"):
            api._request("GET", "http://api.github.com/cleartext")

    def test_client_rejects_a_cleartext_api_root(self) -> None:
        with self.assertRaisesRegex(ValueError, "must use HTTPS"):
            GitHubApi("test-token", "http://api.github.com")

    def test_redirect_handler_never_forwards_the_token(self) -> None:
        handler = RejectRedirects()

        with self.assertRaisesRegex(RuntimeError, "through a redirect"):
            handler.redirect_request(
                Mock(),
                Mock(),
                302,
                "Found",
                {},
                "https://example.test/steal",
            )

    def test_pagination_cycle_fails_instead_of_waiting_forever(self) -> None:
        api = GitHubApi("test-token")
        repeated_page = (
            "https://api.github.com/repos/matrixorigin/Astra/actions/runs?page=2"
        )
        api._request = Mock(  # type: ignore[method-assign]
            return_value=(
                json.dumps({"workflow_runs": []}).encode(),
                {"Link": f'<{repeated_page}>; rel="next"'},
            )
        )

        with self.assertRaisesRegex(RuntimeError, "pagination"):
            api.list_runs("matrixorigin/Astra", OLD_SHA)

    def test_cancel_treats_a_completion_race_as_terminal(self) -> None:
        api = GitHubApi("test-token")
        api._request = Mock(  # type: ignore[method-assign]
            side_effect=[
                HTTPError("url", 409, "Conflict", {}, None),
                (json.dumps({"status": "completed"}).encode(), {}),
            ]
        )

        self.assertFalse(api.cancel_run("matrixorigin/Astra", 42))

    def test_cancel_does_not_hide_a_conflict_for_an_active_run(self) -> None:
        api = GitHubApi("test-token")
        api._request = Mock(  # type: ignore[method-assign]
            side_effect=[
                HTTPError("url", 409, "Conflict", {}, None),
                (json.dumps({"status": "in_progress"}).encode(), {}),
            ]
        )

        with self.assertRaisesRegex(RuntimeError, "refused to cancel active"):
            api.cancel_run("matrixorigin/Astra", 42)


class CancelSupersededRunsTests(unittest.TestCase):
    def test_cancels_every_active_status_but_not_completed_runs(self) -> None:
        runs = [
            workflow_run(index, status=status)
            for index, status in enumerate(sorted(ACTIVE_RUN_STATUSES), start=1)
        ]
        runs.append(workflow_run(100, status="completed"))
        api = FakeApi(runs)

        with redirect_stdout(StringIO()):
            found, cancelled = cancel_superseded_runs(
                api,
                "matrixorigin/Astra",
                SupersededHead(101, "fix/runtime", OLD_SHA),
            )

        self.assertEqual(
            (found, cancelled),
            (len(ACTIVE_RUN_STATUSES), len(ACTIVE_RUN_STATUSES)),
        )
        self.assertEqual(api.listed_heads, [OLD_SHA])

    def test_completion_race_is_not_reported_as_a_failed_cancellation(self) -> None:
        api = FakeApi([workflow_run(1), workflow_run(2)])
        api.finished_before_cancel.add(2)

        with redirect_stdout(StringIO()):
            found, cancelled = cancel_superseded_runs(
                api,
                "matrixorigin/Astra",
                SupersededHead(101, "fix/runtime", OLD_SHA),
            )

        self.assertEqual((found, cancelled), (2, 1))
        self.assertEqual(api.cancelled, [1])

    def test_unexpected_cancellation_fanout_fails_before_mutation(self) -> None:
        api = FakeApi(
            [workflow_run(run_id) for run_id in range(1, MAX_SUPERSEDED_RUNS + 2)]
        )

        with self.assertRaisesRegex(RuntimeError, "unexpectedly large"):
            cancel_superseded_runs(
                api,
                "matrixorigin/Astra",
                SupersededHead(101, "fix/runtime", OLD_SHA),
            )

        self.assertEqual(api.cancelled, [])


class ArgumentContractTests(unittest.TestCase):
    def test_option_shaped_head_ref_remains_a_value(self) -> None:
        args = parse_args(
            [
                "--repository=matrixorigin/Astra",
                "--head-repository-id=101",
                "--head-ref=--repository",
                f"--superseded-head-sha={OLD_SHA}",
            ]
        )

        self.assertEqual(args.head_ref, "--repository")


class WorkflowSecurityContractTests(unittest.TestCase):
    def test_controller_executes_only_the_trusted_base_revision(self) -> None:
        workflow = (
            Path(__file__).resolve().parents[2]
            / ".github/workflows/supersede-pr-runs.yml"
        ).read_text(encoding="utf-8")

        for required in (
            "pull_request_target:",
            "actions: write",
            "contents: read",
            "ref: ${{ github.workflow_sha }}",
            "persist-credentials: false",
            "github.event.pull_request.head.repo.full_name != github.repository",
            "SUPERSEDED_HEAD_SHA: ${{ github.event.before }}",
            "python3 scripts/ci/cancel_stale_pr_runs.py",
            '--head-ref="$HEAD_REF"',
        ):
            with self.subTest(required=required):
                self.assertIn(required, workflow)
        permissions = workflow.partition("permissions:\n")[2].partition("\n\n")[0]
        self.assertEqual(permissions, "  actions: write\n  contents: read")
        self.assertEqual(workflow.count("uses: actions/checkout@"), 1)
        refs = [
            line.strip()
            for line in workflow.splitlines()
            if line.strip().startswith("ref:")
        ]
        self.assertEqual(refs, ["ref: ${{ github.workflow_sha }}"])
        self.assertNotIn("secrets.", workflow)
        self.assertNotRegex(workflow, r"(?m)^\s*concurrency:")

    def test_pull_request_concurrency_isolated_by_pr_number(self) -> None:
        root = Path(__file__).resolve().parents[2]
        expected = "${{ github.event.pull_request.number"
        for relative in (
            ".github/workflows/pr-title.yml",
            ".github/workflows/static-checks.yml",
            ".github/workflows/test.yml",
        ):
            with self.subTest(workflow=relative):
                self.assertIn(expected, (root / relative).read_text(encoding="utf-8"))

    def test_pr_jobs_do_not_resist_workflow_cancellation(self) -> None:
        root = Path(__file__).resolve().parents[2]
        for relative in (
            ".github/workflows/static-checks.yml",
            ".github/workflows/test.yml",
        ):
            workflow = (root / relative).read_text(encoding="utf-8")
            job_level_always = [
                line
                for line in workflow.splitlines()
                if line.startswith("    if:") and "always()" in line
            ]
            with self.subTest(workflow=relative):
                self.assertEqual(job_level_always, [])
                self.assertIn("if: ${{ !cancelled()", workflow)


if __name__ == "__main__":
    unittest.main()
