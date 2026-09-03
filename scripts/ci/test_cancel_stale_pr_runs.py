#!/usr/bin/env python3

from __future__ import annotations

from contextlib import redirect_stdout
from io import StringIO
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

from cancel_stale_pr_runs import (  # noqa: E402
    ACTIVE_RUN_STATUSES,
    GitHubApi,
    MAX_STALE_RUNS,
    PullRequestHead,
    cancel_stale_runs,
    select_stale_runs,
)


CURRENT_SHA = "b" * 40
OLD_SHA = "a" * 40


def workflow_run(
    run_id: int,
    *,
    sha: str = OLD_SHA,
    repository_id: int = 101,
    branch: str = "fix/runtime",
) -> dict[str, object]:
    return {
        "id": run_id,
        "name": "Test Suite",
        "head_sha": sha,
        "head_branch": branch,
        "head_repository": {"id": repository_id, "full_name": "contributor/Astra"},
    }


class FakeApi:
    def __init__(self, *, live_sha: str = CURRENT_SHA, state: str = "open") -> None:
        self.pull_request = {
            "state": state,
            "head": {
                "sha": live_sha,
                "ref": "fix/runtime",
                "repo": {"id": 101, "full_name": "contributor/Astra"},
            },
        }
        self.runs_by_status: dict[str, list[dict[str, object]]] = {
            status: [] for status in ACTIVE_RUN_STATUSES
        }
        self.cancelled: list[int] = []
        self.finished_before_cancel: set[int] = set()

    def get_pull_request(self, repository: str, number: int) -> dict[str, object]:
        self.requested_pull = (repository, number)
        return self.pull_request

    def list_runs(self, repository: str, status: str) -> list[dict[str, object]]:
        return self.runs_by_status[status]

    def cancel_run(self, repository: str, run_id: int) -> bool:
        if run_id in self.finished_before_cancel:
            return False
        self.cancelled.append(run_id)
        return True


class SelectStaleRunsTests(unittest.TestCase):
    def test_selects_only_prior_heads_of_the_exact_fork_ref(self) -> None:
        head = PullRequestHead(101, "Contributor/Astra", "fix/runtime", CURRENT_SHA)
        runs = [
            workflow_run(1),
            workflow_run(2, sha=CURRENT_SHA),
            workflow_run(3, repository_id=202),
            workflow_run(4, branch="fix/other"),
            {"id": 5, "head_sha": OLD_SHA, "head_branch": "fix/runtime"},
        ]

        self.assertEqual([run["id"] for run in select_stale_runs(runs, head)], [1])

    def test_deduplicates_a_run_observed_during_status_transition(self) -> None:
        head = PullRequestHead(101, "contributor/Astra", "fix/runtime", CURRENT_SHA)
        run = workflow_run(7)

        self.assertEqual([item["id"] for item in select_stale_runs([run, run], head)], [7])


class GitHubApiTests(unittest.TestCase):
    def test_list_runs_follows_pagination(self) -> None:
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
            [run["id"] for run in api.list_runs("matrixorigin/Astra", "in_progress")],
            [1, 2],
        )

    def test_request_rejects_a_foreign_origin_before_sending_the_token(self) -> None:
        api = GitHubApi("test-token")

        with self.assertRaisesRegex(RuntimeError, "another origin"):
            api._request("GET", "https://example.test/steal")


class CancelStaleRunsTests(unittest.TestCase):
    def test_cancels_prior_runs_across_every_active_status(self) -> None:
        api = FakeApi()
        for index, status in enumerate(ACTIVE_RUN_STATUSES, start=1):
            api.runs_by_status[status] = [workflow_run(index)]
        api.runs_by_status["pending"].append(workflow_run(100, sha=CURRENT_SHA))

        found, cancelled = cancel_stale_runs(api, "matrixorigin/Astra", 42, CURRENT_SHA)

        self.assertEqual(
            (found, cancelled),
            (len(ACTIVE_RUN_STATUSES), len(ACTIVE_RUN_STATUSES)),
        )
        self.assertEqual(api.cancelled, list(range(1, len(ACTIVE_RUN_STATUSES) + 1)))

    def test_stale_controller_cannot_cancel_a_newer_head(self) -> None:
        api = FakeApi(live_sha="c" * 40)
        api.runs_by_status["in_progress"] = [workflow_run(1, sha=CURRENT_SHA)]

        found, cancelled = cancel_stale_runs(api, "matrixorigin/Astra", 42, CURRENT_SHA)

        self.assertEqual((found, cancelled), (0, 0))
        self.assertEqual(api.cancelled, [])

    def test_closed_pull_request_event_is_a_safe_noop(self) -> None:
        api = FakeApi(state="closed")
        api.runs_by_status["in_progress"] = [workflow_run(1)]

        found, cancelled = cancel_stale_runs(api, "matrixorigin/Astra", 42, CURRENT_SHA)

        self.assertEqual((found, cancelled), (0, 0))
        self.assertEqual(api.cancelled, [])

    def test_completion_race_is_not_reported_as_a_failed_cancellation(self) -> None:
        api = FakeApi()
        api.runs_by_status["in_progress"] = [workflow_run(1), workflow_run(2)]
        api.finished_before_cancel.add(2)

        with redirect_stdout(StringIO()):
            found, cancelled = cancel_stale_runs(api, "matrixorigin/Astra", 42, CURRENT_SHA)

        self.assertEqual((found, cancelled), (2, 1))
        self.assertEqual(api.cancelled, [1])

    def test_unexpected_cancellation_fanout_fails_before_mutation(self) -> None:
        api = FakeApi()
        api.runs_by_status["queued"] = [
            workflow_run(run_id) for run_id in range(1, MAX_STALE_RUNS + 2)
        ]

        with self.assertRaisesRegex(RuntimeError, "unexpectedly large"):
            cancel_stale_runs(api, "matrixorigin/Astra", 42, CURRENT_SHA)

        self.assertEqual(api.cancelled, [])


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
            "pull-requests: read",
            "ref: ${{ github.event.pull_request.base.sha }}",
            "persist-credentials: false",
            "python3 scripts/ci/cancel_stale_pr_runs.py",
        ):
            with self.subTest(required=required):
                self.assertIn(required, workflow)
        self.assertNotIn("ref: ${{ github.event.pull_request.head.sha }}", workflow)
        self.assertNotIn("secrets.", workflow)

    def test_pull_request_concurrency_isolated_by_pr_number(self) -> None:
        root = Path(__file__).resolve().parents[2]
        expected = "${{ github.event.pull_request.number"
        for relative in (
            ".github/workflows/pr-title.yml",
            ".github/workflows/static-checks.yml",
            ".github/workflows/test.yml",
            ".github/workflows/supersede-pr-runs.yml",
        ):
            with self.subTest(workflow=relative):
                self.assertIn(expected, (root / relative).read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
