#!/usr/bin/env python3
"""Exercise dispatch/queue/fallback against the production entrypoints."""

from copy import deepcopy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import macos_dispatch as dispatch


ROOT = Path(__file__).resolve().parents[2]
REPO = "matrixorigin/Astra"
BASE, HEAD, MERGE = "a" * 40, "b" * 40, "c" * 40


class Clock:
    def __init__(self):
        self.now = 0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class ApiFixture:
    def __init__(self):
        self.clock = Clock()
        self.run = {"id": 1, "run_attempt": 1, "name": "Test Suite",
            "path": ".github/workflows/test.yml", "event": "pull_request",
            "display_title": f"Test Suite pr=1126 base={BASE} head={HEAD} merge={MERGE}",
            "head_sha": HEAD, "status": "in_progress", "pull_requests": [],
            "head_repository": {"id": 42}, "repository": {"id": 7}}
        self.run["name"] = self.run["display_title"]
        self.pr = {"state": "open", "user": {"id": 52204121},
                   "head": {"sha": HEAD, "repo": {"id": 42}}}
        self.parents = [BASE, HEAD]
        self.src = dispatch.Source(1, 1, MERGE)
        self.enabled = True
        self.assign_at, self.finish_at = 0, 30
        self.run_conclusion, self.test_conclusion = "success", "success"
        self.gate_end_at = None
        self.wait_end_at = None
        self.gate_start_at = 0
        self.gate_conclusion = "success"
        self.source_end_at = None
        self.other_runs = []
        self.posts, self.cancelled = [], []
        self.calls = []

    def get(self, path):
        self.calls.append(path)
        if path.endswith("/actions/runs/1"):
            run = deepcopy(self.run)
            if self.source_end_at is not None and self.clock() >= self.source_end_at:
                run["status"] = "completed"
            return run
        if path.endswith("/pulls/1126"):
            return deepcopy(self.pr)
        if path.endswith("/commits/" + MERGE):
            return {"parents": [{"sha": sha} for sha in self.parents]}
        raise AssertionError(path)

    def worker(self):
        status = "completed" if self.clock() >= self.finish_at else "in_progress"
        return {"id": 10, "run_attempt": 1, "display_title": self.src.title,
                "path": ".github/workflows/macos-mini.yml", "event": "workflow_dispatch",
                "status": status, "conclusion": self.run_conclusion if status == "completed" else None}

    def collection(self, path, key):
        self.calls.append(path)
        if "/workflows/macos-mini.yml/runs?" in path:
            return ([self.worker()] if self.enabled else []) + deepcopy(self.other_runs)
        if "/runs/1/attempts/1/jobs?" in path:
            completed = self.gate_end_at is not None and self.clock() >= self.gate_end_at
            status = "in_progress" if self.clock() >= self.gate_start_at else "queued"
            wait_status = "completed" if completed or (self.wait_end_at is not None and self.clock() >= self.wait_end_at) else status
            return [{"name": dispatch.GATE, "status": "completed" if completed else status,
                     "conclusion": self.gate_conclusion if completed else None,
                     "steps": [{"name": dispatch.WAIT_STEP, "status": wait_status}]}]
        if "/runs/10/attempts/1/jobs?" in path:
            complete = self.clock() >= self.finish_at
            assigned = self.clock() >= self.assign_at
            steps = [{"name": dispatch.TEST_STEP, "conclusion": self.test_conclusion}] if complete else []
            if complete and self.test_conclusion is None:
                steps.append({"name": "Prepare Rust", "conclusion": "failure"})
            return [{"name": "execute", "runner_id": 99 if assigned else 0,
                     "status": "completed" if complete else "in_progress" if assigned else "queued",
                     "steps": steps}]
        raise AssertionError(path)

    def post(self, path, data):
        self.posts.append((path, data))
        self.enabled = True

    def cancel_run(self, repo, run_id):
        self.cancelled.append(run_id)
        return True


class SourceTests(unittest.TestCase):
    def test_dispatch_serializes_json_through_the_shared_origin_checked_client(self):
        api = dispatch.Api("fixture")
        data = {"ref": "main", "inputs": {"source_run_id": "1"}}
        with patch.object(api, "_opener") as opener:
            response = opener.open.return_value.__enter__.return_value
            response.read.return_value = b""
            response.headers = {}
            api.post(f"repos/{REPO}/actions/workflows/macos-mini.yml/dispatches", data)
            request = opener.open.call_args.args[0]
            self.assertEqual(request.get_method(), "POST")
            self.assertEqual(json.loads(request.data), data)
            self.assertEqual(request.get_header("Content-type"), "application/json")
            self.assertEqual(opener.open.call_args.kwargs["timeout"], 10)

    def test_empty_fork_pr_association_uses_verified_merge_tuple(self):
        api = ApiFixture()
        self.assertEqual(dispatch.source(api, REPO, 1, 1), api.src)
        self.assertFalse(any("members" in call for call in api.calls))

    def test_static_and_dynamic_run_names_use_the_workflow_path_as_identity(self):
        for name in ("Test Suite", ApiFixture().run["display_title"]):
            api = ApiFixture()
            api.run["name"] = name
            self.assertEqual(dispatch.source(api, REPO, 1, 1), api.src)
        api.run["path"] = ".github/workflows/other.yml"
        with self.assertRaises(dispatch.Unavailable):
            dispatch.source(api, REPO, 1, 1)

    def test_wrong_parent_order_head_repository_author_and_attempt_are_rejected(self):
        mutations = (
            lambda a: setattr(a, "parents", [HEAD, BASE]),
            lambda a: a.pr["head"].update(sha="d" * 40),
            lambda a: a.pr["head"]["repo"].update(id=43),
            lambda a: a.pr["user"].update(id=999999999),
            lambda a: a.run.update(run_attempt=2),
        )
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                api = ApiFixture()
                mutate(api)
                with self.assertRaises(dispatch.Unavailable):
                    dispatch.source(api, REPO, 1, 1)

    def test_main_develop_pushes_cannot_enter_the_pr_runner(self):
        for branch in ("main", "develop"):
            api = ApiFixture()
            api.run.update(event="push", head_branch=branch, head_repository={"id": 7},
                display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
            with self.assertRaises(dispatch.Unavailable):
                dispatch.source(api, REPO, 1, 1)


class WaitTests(unittest.TestCase):
    def wait(self, api):
        return dispatch.wait_for_mini(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)

    def test_offline_queued_worker_has_bounded_wait(self):
        api = ApiFixture()
        api.assign_at = api.finish_at = 100000
        self.assertEqual(self.wait(api), "hosted")
        self.assertEqual(api.clock(), dispatch.QUEUE_SECONDS)

    def test_missing_dispatch_has_the_same_bounded_wait(self):
        api = ApiFixture()
        api.enabled = False
        self.assertEqual(self.wait(api), "hosted")
        self.assertEqual(api.clock(), dispatch.QUEUE_SECONDS)

    def test_assigned_worker_may_complete_after_queue_deadline(self):
        api = ApiFixture()
        api.assign_at, api.finish_at = 170, 200
        self.assertEqual(self.wait(api), "success")

    def test_execution_deadline_and_agent_loss_use_hosted(self):
        api = ApiFixture()
        api.finish_at = 100000
        self.assertEqual(self.wait(api), "hosted")
        self.assertEqual(api.clock(), dispatch.EXECUTION_SECONDS)

    def test_one_hour_attempt_stays_below_half_the_default_rest_budget(self):
        api = ApiFixture()
        api.finish_at = 100000
        self.assertEqual(self.wait(api), "hosted")
        read_calls = len(api.calls)
        api = ApiFixture()
        api.finish_at = 100000
        dispatch.schedule(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)
        self.assertLess(read_calls + len(api.calls), 500)

    def test_actual_test_failure_never_becomes_hosted_retry(self):
        api = ApiFixture()
        api.run_conclusion = api.test_conclusion = "failure"
        self.assertEqual(self.wait(api), "failure")

    def test_failed_run_with_incomplete_test_metadata_is_not_retried(self):
        api = ApiFixture()
        api.finish_at = 0
        api.run_conclusion, api.test_conclusion = "failure", "pending"
        self.assertEqual(self.wait(api), "failure")

    def test_known_test_failure_wins_before_post_job_cleanup_finishes(self):
        api = ApiFixture()
        api.finish_at = 0
        api.test_conclusion = "failure"
        run = api.worker()
        run.update(status="in_progress", conclusion=None)
        self.assertEqual(dispatch.worker_state(api, REPO, run)[0], "failure")

    def test_worker_lookup_filters_history_to_the_source_creation_window(self):
        api = ApiFixture()
        src = dispatch.Source(1, 1, MERGE, "2026-10-24T08:00:00Z")
        dispatch.workers(api, REPO, src)
        from urllib.parse import parse_qs, urlsplit
        self.assertEqual(parse_qs(urlsplit(api.calls[-1]).query)["created"], [">=2026-10-24T08:00:00Z"])

    def test_low_disk_checkout_toolchain_skips_and_cancellation_are_not_success(self):
        for conclusion, test in (("failure", None), ("success", "skipped"),
                                 ("cancelled", "cancelled"), ("timed_out", "cancelled")):
            with self.subTest(conclusion=conclusion, test=test):
                api = ApiFixture()
                api.run_conclusion, api.test_conclusion = conclusion, test
                self.assertEqual(self.wait(api), "hosted")

    def test_same_sha_different_source_run_and_attempt_cannot_supply_results(self):
        for title in (dispatch.Source(2, 1, MERGE).title, dispatch.Source(1, 2, MERGE).title):
            api = ApiFixture()
            api.enabled = False
            other = api.worker()
            other.update(id=20, status="completed", conclusion="success", display_title=title)
            api.other_runs = [other]
            self.assertEqual(self.wait(api), "hosted")
            self.assertEqual(api.clock(), dispatch.QUEUE_SECONDS)

    def test_parent_cancellation_does_not_reuse_old_mini_result(self):
        api = ApiFixture()
        api.source_end_at = 0
        with self.assertRaises(dispatch.Unavailable):
            self.wait(api)

    def test_worker_individual_rerun_is_unavailable(self):
        api = ApiFixture()
        worker = api.worker()
        worker["run_attempt"] = 2
        with self.assertRaises(dispatch.Unavailable):
            dispatch.worker_state(api, REPO, worker)


class SchedulerTests(unittest.TestCase):
    def run_scheduler(self, api):
        dispatch.schedule(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)

    def test_offline_worker_is_cancelled_without_touching_other_pr(self):
        api = ApiFixture()
        api.enabled = False
        api.assign_at = api.finish_at = 100000
        other = api.worker()
        other.update(id=20, display_title=dispatch.Source(2, 1, MERGE).title)
        api.other_runs = [other]
        self.run_scheduler(api)
        self.assertEqual(api.cancelled, [10])
        self.assertEqual(api.clock(), dispatch.QUEUE_SECONDS)
        self.assertEqual(api.posts[0][1], {"ref": "main", "inputs": {
            "source_run_id": "1", "source_attempt": "1", "source_sha": MERGE}})

    def test_completed_gate_cannot_dispatch_a_late_worker(self):
        api = ApiFixture()
        api.enabled = False
        api.gate_end_at = 0
        self.run_scheduler(api)
        self.assertEqual(api.posts, [])

    def test_finished_wait_step_blocks_dispatch_during_post_job_cleanup(self):
        api = ApiFixture()
        api.enabled = False
        api.wait_end_at = 0
        self.run_scheduler(api)
        self.assertEqual(api.posts, [])

    def test_gate_start_delay_does_not_consume_the_worker_queue_budget(self):
        api = ApiFixture()
        api.enabled = False
        api.gate_start_at, api.assign_at, api.finish_at = 100, 270, 300
        self.run_scheduler(api)
        self.assertEqual(api.cancelled, [])
        self.assertGreaterEqual(api.clock(), 300)
        self.assertLessEqual(api.clock(), 300 + dispatch.RUNNING_POLL_SECONDS)

    def test_cancel_failure_does_not_prevent_other_owned_workers_from_cancelling(self):
        api = ApiFixture()
        duplicate = api.worker()
        duplicate["id"] = 11
        api.other_runs = [duplicate]
        original = api.cancel_run

        def cancel(repo, run_id):
            original(repo, run_id)
            if run_id == 10:
                raise RuntimeError("temporary API failure")

        api.cancel_run = cancel
        with self.assertRaises(dispatch.Unavailable):
            dispatch.cancel_workers(api, REPO, api.src)
        self.assertEqual(api.cancelled, [10, 11])

    def test_source_cancel_and_gate_fallback_cancel_only_the_owned_worker(self):
        for field in ("source_end_at", "gate_end_at"):
            api = ApiFixture()
            api.finish_at = 100000
            setattr(api, field, 20)
            self.run_scheduler(api)
            self.assertEqual(api.cancelled, [10])
            self.assertLessEqual(api.clock(), 20 + dispatch.RUNNING_POLL_SECONDS)

    def test_success_or_real_test_failure_does_not_cancel_completed_worker(self):
        for result in ("success", "failure"):
            api = ApiFixture()
            api.run_conclusion = api.test_conclusion = result
            self.run_scheduler(api)
            self.assertEqual(api.cancelled, [])


class EntrypointTests(unittest.TestCase):
    def invoke(self, api, mode, *, worker_attempt="1"):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            args = ["macos_dispatch.py", mode, "--run-id", "1", "--attempt", "1", "--sha", MERGE]
            with patch.object(dispatch, "Api", return_value=api), patch.object(sys, "argv", args), \
                 patch.dict(os.environ, {"GITHUB_REPOSITORY": REPO, "GH_TOKEN": "fixture",
                     "GITHUB_OUTPUT": str(output), "GITHUB_RUN_ATTEMPT": worker_attempt}, clear=True):
                status = dispatch.main()
            return status, output.read_text() if output.exists() else ""

    def test_unavailable_api_and_missing_controller_fail_open_to_actual_hosted_tests(self):
        api = ApiFixture()
        with patch.object(api, "get", side_effect=RuntimeError("unavailable")):
            self.assertEqual(self.invoke(api, "wait"), (0, "result=hosted\n"))

    def test_failure_is_published_as_failure_not_fallback(self):
        api = ApiFixture()
        api.finish_at = 0
        api.run_conclusion = api.test_conclusion = "failure"
        self.assertEqual(self.invoke(api, "wait"), (0, "result=failure\n"))

    def test_completion_cancels_orphan_even_after_pr_close_or_attempt_change(self):
        api = ApiFixture()
        api.run.update(status="completed", run_attempt=2)
        api.pr["state"] = "closed"
        self.assertEqual(self.invoke(api, "cleanup")[0], 0)
        self.assertEqual(api.cancelled, [10])

    def test_late_or_reused_worker_rejected_before_candidate_checkout(self):
        for completed, attempt in ((True, "1"), (False, "2")):
            api = ApiFixture()
            if completed:
                api.gate_end_at = 0
            self.assertEqual(self.invoke(api, "admit", worker_attempt=attempt)[0], 1)
        workflow = (ROOT / ".github/workflows/macos-mini.yml").read_text()
        self.assertLess(workflow.index("macos_dispatch.py admit"), workflow.index("ref: ${{ inputs.source_sha }}"))
        self.assertIn("cache-mode: read", workflow)
        self.assertNotIn("actions: write", workflow)

    def test_worker_cannot_start_during_parent_post_job_cleanup(self):
        api = ApiFixture()
        api.wait_end_at = 0
        self.assertEqual(self.invoke(api, "admit")[0], 1)


class LaneTests(unittest.TestCase):
    def test_bundle_executes_all_real_lane_commands_and_propagates_failure(self):
        for fail in (False, True):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                tools = root / "bin"
                tools.mkdir()
                log = root / "commands"
                for command in ("cargo", "npm"):
                    program = tools / command
                    program.write_text('#!/bin/bash\nprintf "%s %s\\n" "${0##*/}" "$*" >> "$COMMANDS"\n'
                        'if [[ "${FAIL_CARGO:-}" == 1 && "${0##*/}" == cargo ]]; then exit 17; fi\n')
                    program.chmod(0o755)
                # Keep the native Git probe real, but use a disposable repository.
                subprocess.run(["git", "init", "-q", str(root)], check=True)
                result = subprocess.run(["bash", str(ROOT / "scripts/ci/run-macos-lanes.sh"), "all"],
                    cwd=root, env={**os.environ, "PATH": str(tools) + ":" + os.environ["PATH"],
                        "COMMANDS": str(log), "FAIL_CARGO": "1" if fail else "0"}, capture_output=True, text=True)
                calls = log.read_text().splitlines()
                if fail:
                    self.assertEqual(result.returncode, 17)
                    self.assertEqual(len(calls), 1)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue(any("--features crossterm/use-dev-tty tui::terminal_startup" in c for c in calls))
                    self.assertEqual(sum("vendor/crossterm" in c for c in calls), 2)
                    self.assertTrue(any("workspace_observation::tests:: -- --test-threads=1" in c for c in calls))
                    self.assertTrue(any("npm test --prefix scripts/tui-reflow" in c for c in calls))
                    self.assertIn("crossterm/use-dev-tty", calls[-1])


if __name__ == "__main__":
    unittest.main()
