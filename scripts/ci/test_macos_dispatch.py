#!/usr/bin/env python3
"""Exercise dispatch/queue/fallback against the production entrypoints."""

from copy import deepcopy
from contextlib import contextmanager
from http.client import IncompleteRead
from io import BytesIO, StringIO
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

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


@contextmanager
def api_clock():
    clock = Clock()
    with patch.object(dispatch.time, "monotonic", side_effect=clock), \
         patch.object(dispatch.time, "time", side_effect=lambda: 1000 + clock()), \
         patch.object(dispatch.time, "sleep", side_effect=clock.sleep):
        yield clock


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
                   "head": {"sha": HEAD, "repo": {"id": 42}},
                   "base": {"ref": "main", "repo": {"id": 7}}}
        self.parents = [BASE, HEAD]
        self.tip = BASE
        self.compare_status = "ahead"
        self.src = dispatch.Source(1, 1, MERGE, pr_number=1126, head_sha=HEAD, head_repo_id=42)
        self.enabled = True
        self.assign_at, self.finish_at = 0, 30
        self.run_conclusion, self.test_conclusion = "success", "success"
        self.gate_end_at = None
        self.wait_end_at = None
        self.gate_start_at = 0
        self.gate_conclusion = "success"
        self.source_end_at = None
        self.pr_change_at = None
        self.other_runs = []
        self.posts, self.cancelled = [], []
        self.calls = []
        self.timed_calls = []
        self.deadline = None

    def set_deadline(self, deadline, *, observe=False):
        self.deadline = deadline

    def get(self, path):
        self.calls.append(path)
        self.timed_calls.append(self.clock())
        if path.endswith("/actions/runs/1"):
            run = deepcopy(self.run)
            if self.source_end_at is not None and self.clock() >= self.source_end_at:
                run["status"] = "completed"
            return run
        if path.endswith("/pulls/1126"):
            pr = deepcopy(self.pr)
            if self.pr_change_at is not None and self.clock() >= self.pr_change_at:
                pr["head"]["sha"] = "f" * 40
            return pr
        if path.endswith("/commits/" + MERGE):
            return {"parents": [{"sha": sha} for sha in self.parents]}
        if "/git/ref/heads/" in path:
            return {"object": {"sha": self.tip}}
        if "/compare/" in path:
            return {"status": self.compare_status}
        raise AssertionError(path)

    def worker(self):
        status = "completed" if self.clock() >= self.finish_at else "in_progress"
        return {"id": 10, "run_attempt": 1, "display_title": self.src.title,
                "path": ".github/workflows/macos-mini.yml", "event": "workflow_dispatch",
                "status": status, "conclusion": self.run_conclusion if status == "completed" else None}

    def collection(self, path, key):
        self.calls.append(path)
        self.timed_calls.append(self.clock())
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
        self.dispatch_time, self.dispatch_deadline = self.clock(), self.deadline
        self.enabled = True

    def cancel_run(self, repo, run_id):
        self.cancelled.append(run_id)
        return True


@contextmanager
def real_api(fixture):
    """Replace only network I/O; keep production deadlines, retries and cleanup."""
    api = dispatch.Api("fixture")
    api.clock = fixture.clock

    def transport(method, path, **kwargs):
        if method == "POST":
            if path.endswith("/cancel"):
                fixture.cancel_run(REPO, int(path.split("/")[-2]))
            else:
                fixture.post(path, kwargs["data"])
            return b"", {}
        if "/workflows/" in path:
            value = {"workflow_runs": fixture.collection(path, "workflow_runs")}
        elif "/attempts/" in path:
            value = {"jobs": fixture.collection(path, "jobs")}
        else:
            value = fixture.get(path)
        return json.dumps(value).encode(), {}

    with patch.object(dispatch.GitHubApi, "_request", side_effect=transport), \
         patch.object(dispatch.time, "monotonic", side_effect=fixture.clock), \
         patch.object(dispatch.time, "sleep", side_effect=fixture.clock.sleep):
        yield api


class SourceTests(unittest.TestCase):
    def test_conditional_get_uses_only_a_server_validated_cached_body(self):
        api = dispatch.Api("fixture")
        headers = {"ETag": '"version-1"'}
        responses = [(b'{"id": 1}', headers),
                     HTTPError("https://api.github.com/fixture", 304, "unchanged", {}, None),
                     (b'{"id": 2}', {"ETag": '"version-2"'})]
        with patch.object(dispatch.GitHubApi, "_request", side_effect=responses) as request:
            self.assertEqual(api.get("fixture"), {"id": 1})
            self.assertEqual(api.get("fixture"), {"id": 1})
            self.assertEqual(request.call_args.kwargs["etag"], '"version-1"')
            self.assertEqual(api.get("fixture"), {"id": 2})
        with patch.object(dispatch.GitHubApi, "_request", side_effect=URLError("offline")), api_clock():
            with self.assertRaises(URLError):
                api.get("fixture")
        self.assertEqual(api._conditional["fixture"][0], b'{"id": 2}')

    def test_conditional_collection_keeps_pagination_and_rejects_uncached_304(self):
        api = dispatch.Api("fixture")
        first = {"ETag": '"page-1"', "Link": '<https://api.github.com/page2>; rel="next"'}
        second = {"ETag": '"page-2"'}
        responses = [(b'{"workflow_runs": [{"id": 1}]}', first),
                     (b'{"workflow_runs": [{"id": 2}]}', second),
                     HTTPError("https://api.github.com/page1", 304, "unchanged", {}, None),
                     HTTPError("https://api.github.com/page2", 304, "unchanged", {}, None)]
        with patch.object(dispatch.GitHubApi, "_request", side_effect=responses):
            self.assertEqual(api.collection("page1", "workflow_runs"), [{"id": 1}, {"id": 2}])
            self.assertEqual(api.collection("page1", "workflow_runs"), [{"id": 1}, {"id": 2}])
        with patch.object(dispatch.GitHubApi, "_request", side_effect=HTTPError("https://api.github.com/unknown", 304, "unchanged", {}, None)):
            with self.assertRaises(dispatch.Unavailable):
                api.get("unknown")

    def test_rate_limits_obey_headers_and_secondary_limit_backoff(self):
        cases = [
            (429, {"Retry-After": "75"}, b"", 75),
            (403, {"Retry-After": "95"}, b"", 95),
            (403, {"X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "1100"}, b"", 101),
            (429, {"X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "1100", "Retry-After": "150"}, b"", 150),
            (429, {"Retry-After": "Thu, 01 Jan 1970 00:18:20 GMT"}, b"", 101),
            (403, {}, b'{"message": "You have exceeded a secondary rate limit."}', 60),
        ]
        for code, headers, body, expected in cases:
            api = dispatch.Api("fixture")
            error = HTTPError("https://api.github.com/fixture", code, "limited", headers, BytesIO(body))
            with self.subTest(code=code, headers=headers), api_clock() as clock, \
                 patch.object(dispatch.GitHubApi, "_request", side_effect=[error, (b'{"id": 1}', {})]):
                api.set_deadline(900)
                self.assertEqual(api.get("fixture"), {"id": 1})
                self.assertEqual(clock(), expected)
        api = dispatch.Api("fixture")
        errors = [HTTPError("https://api.github.com/fixture", 429, "limited", {}, None) for _ in range(3)]
        with api_clock() as clock, patch.object(dispatch.GitHubApi, "_request", side_effect=errors) as request:
            with self.assertRaises(HTTPError):
                api.get("fixture")
            self.assertEqual(clock(), 180)
            self.assertEqual(request.call_count, 3)

    def test_backoff_cannot_overrun_budget_or_skip_a_source_update(self):
        api = dispatch.Api("fixture")
        error = HTTPError("https://api.github.com/fixture", 429, "limited", {"Retry-After": "100"}, None)
        with api_clock() as clock, patch.object(dispatch.GitHubApi, "_request", side_effect=error) as request:
            api.set_deadline(90)
            with self.assertRaises(dispatch.Unavailable):
                api.get("fixture")
            self.assertEqual(clock(), 0)
            self.assertEqual(request.call_count, 1)

        fixture = ApiFixture()
        fixture.finish_at = 0
        original = fixture.collection

        def collection(path, key):
            result = original(path, key)
            if "/runs/10/attempts/1/jobs?" in path:
                fixture.pr["head"]["sha"] = "d" * 40
            return result

        fixture.collection = collection
        with self.assertRaises(dispatch.Superseded):
            dispatch.wait_for_mini(fixture, REPO, fixture.src, clock=fixture.clock, sleep=fixture.clock.sleep)

    def test_quota_diagnostic_outputs_only_numeric_selected_headers(self):
        api = dispatch.Api("fixture")
        headers = {"X-RateLimit-Limit": "1000", "X-RateLimit-Remaining": "998", "X-RateLimit-Reset": "2000",
                   "Set-Cookie": "private-fixture", "Authorization": "private-fixture"}
        from contextlib import redirect_stdout
        stream = StringIO()
        with patch.object(dispatch.GitHubApi, "_request", return_value=(b'{}', headers)), redirect_stdout(stream):
            api.get("fixture")
            api.get("fixture")
        self.assertEqual(stream.getvalue().count("GitHub API quota:"), 1)
        self.assertIn("limit=1000, remaining=998, reset=2000", stream.getvalue())
        self.assertNotIn("private-fixture", stream.getvalue())
        api = dispatch.Api("fixture")
        with patch.object(dispatch.GitHubApi, "_request", return_value=(b'{}', headers)), \
             patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": "fixture"}), \
             patch.object(Path, "open", side_effect=OSError("unwritable")), redirect_stdout(StringIO()):
            self.assertEqual(api.get("fixture"), {})

    def test_transient_reads_retry_but_permissions_and_dispatch_do_not(self):
        for error in (URLError("temporary"), TimeoutError(), ConnectionResetError(),
                      IncompleteRead(b"partial"),
                      HTTPError("https://api.github.com/fixture", 503, "temporary", {}, None)):
            api = dispatch.Api("fixture")
            with self.subTest(error=type(error).__name__), \
                 patch.object(dispatch.GitHubApi, "_request", side_effect=[error, (b'{"id": 1}', {})]) as request, \
                 api_clock() as clock:
                self.assertEqual(api.get("fixture"), {"id": 1})
                self.assertEqual(request.call_count, 2)
                self.assertEqual(clock(), 2)
        for code in (401, 403, 404):
            api = dispatch.Api("fixture")
            with patch.object(dispatch.GitHubApi, "_request", side_effect=HTTPError("https://api.github.com/fixture", code, "denied", {}, None)) as request:
                with self.assertRaises(HTTPError):
                    api.get("fixture")
                self.assertEqual(request.call_count, 1)
        api = dispatch.Api("fixture")
        with patch.object(dispatch.GitHubApi, "_request", side_effect=URLError("temporary")) as request:
            with self.assertRaises(URLError):
                api.post("dispatches", {})
            self.assertEqual(request.call_count, 1)

    def test_transient_read_retries_are_bounded_and_cover_collections(self):
        api = dispatch.Api("fixture")
        with patch.object(dispatch.GitHubApi, "_request", side_effect=URLError("temporary")) as request, \
             api_clock() as clock:
            with self.assertRaises(URLError):
                api.collection("fixture", "workflow_runs")
            self.assertEqual(request.call_count, 3)
            self.assertEqual(clock(), 6)
        # Use a fresh client after the exhausted client's cooldown.
        api = dispatch.Api("fixture")
        with patch.object(dispatch.GitHubApi, "_request", side_effect=[URLError("temporary"), (b'{"workflow_runs": []}', {})]), \
             api_clock():
            self.assertEqual(api.collection("fixture", "workflow_runs"), [])

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

    def test_conditional_header_keeps_the_shared_origin_checked_transport(self):
        api = dispatch.Api("fixture")
        with patch.object(api, "_opener") as opener:
            response = opener.open.return_value.__enter__.return_value
            response.read.return_value = b'{}'
            response.headers = {}
            api._request("GET", f"repos/{REPO}/fixture", etag='"etag"')
            self.assertEqual(opener.open.call_args.args[0].get_header("If-none-match"), '"etag"')
            with self.assertRaises(RuntimeError):
                api._request("GET", "https://example.invalid/fixture", etag='"etag"')
            self.assertEqual(opener.open.call_count, 1)

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

    def test_outdated_event_base_uses_real_merge_parent_on_target_branch(self):
        api = ApiFixture()
        api.parents[0] = "d" * 40
        api.tip = "e" * 40
        self.assertEqual(dispatch.source(api, REPO, 1, 1), api.src)
        self.assertTrue(any(f"/compare/{'d' * 40}...{'e' * 40}" in c for c in api.calls))
        api.compare_status = "diverged"
        with self.assertRaises(dispatch.Unavailable):
            dispatch.source(api, REPO, 1, 1)

    def test_merge_parent_on_another_branch_or_wrong_base_repository_is_rejected(self):
        for status in ("behind", "diverged", None):
            api = ApiFixture()
            api.tip = "e" * 40
            api.compare_status = status
            with self.assertRaises(dispatch.Unavailable):
                dispatch.source(api, REPO, 1, 1)
        api = ApiFixture()
        api.pr["base"]["repo"]["id"] = 8
        with self.assertRaises(dispatch.Unavailable):
            dispatch.source(api, REPO, 1, 1)

    def test_main_push_is_admitted_to_its_own_pool(self):
        api = ApiFixture()
        api.run.update(event="push", head_branch="main", head_repository={"id": 7},
            display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
        self.assertEqual(dispatch.source(api, REPO, 1, 1), dispatch.Source(1, 1, HEAD))

    def test_main_push_requires_exact_sha_and_repository(self):
        for mutation in (
            lambda a: a.run.update(head_repository={"id": 8}),
            lambda a: a.run.update(head_repository={}),
            lambda a: a.run.update(display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={MERGE}"),
            lambda a: a.run.update(head_sha=MERGE),
            lambda a: a.run.update(status="completed"),
        ):
            api = ApiFixture()
            api.run.update(event="push", head_branch="main", head_repository={"id": 7},
                display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
            mutation(api)
            with self.assertRaises(dispatch.Unavailable):
                dispatch.source(api, REPO, 1, 1)

    def test_other_pushes_cannot_enter_the_runner(self):
        for branch in ("develop", "feature"):
            api = ApiFixture()
            api.run.update(event="push", head_branch=branch, head_repository={"id": 7},
                display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
            with self.assertRaises(dispatch.Unavailable):
                dispatch.source(api, REPO, 1, 1)


class WaitTests(unittest.TestCase):
    def wait(self, api):
        return dispatch.wait_for_mini(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)

    def test_updated_pr_stops_while_queued_or_running(self):
        for assigned in (0, 100000):
            api = ApiFixture()
            api.assign_at, api.finish_at = assigned, 100000
            api.pr_change_at = 120
            with self.assertRaises(dispatch.Superseded):
                self.wait(api)
            self.assertLessEqual(api.clock(), 120 + dispatch.RUNNING_POLL_SECONDS)

    def test_full_queue_plus_one_hour_attempt_stays_below_default_rest_budget(self):
        api = ApiFixture()
        api.assign_at = 890
        api.finish_at = 100000
        self.assertEqual(self.wait(api), "hosted")
        read_calls = len(api.calls)
        api = ApiFixture()
        api.assign_at = 890
        api.finish_at = 100000
        dispatch.schedule(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)
        self.assertLess(read_calls + len(api.calls), 400)

    def test_repository_budget_for_one_active_and_four_queued_prs(self):
        # Count the rolling repository load, assuming every response is 200
        # (no ETag savings), and include 20 startup/admission/write requests per
        # source. This is a stated workload envelope, not unlimited concurrency.
        calls = []
        for index in range(5):
            for role in (dispatch.wait_for_mini, dispatch.schedule):
                api = ApiFixture()
                api.assign_at = 0 if index == 0 else 100000
                api.finish_at = 100000
                role(api, REPO, api.src, clock=api.clock, sleep=api.clock.sleep)
                calls.extend(api.timed_calls)
            calls.extend([0] * 20)
        peak = max(sum(start <= call < start + 3600 for call in calls) for start in set(calls))
        self.assertLess(peak, 800, "Leave quota headroom for the repository's other workflows")

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


class LifecycleTests(unittest.TestCase):
    def test_final_observation_and_fixed_execution_budget_use_real_api(self):
        for role in (dispatch.wait_for_mini, dispatch.schedule):
            # Late assignment, missing/unassigned worker, and exact terminal boundary.
            for enabled, assigned, finished, conclusion, expected_time in (
                (True, 890, 950, "success", 1020),
                (False, 100000, 100000, "success", 900),
                (True, 100000, 100000, "success", 900),
                (True, 0, 3600, "success", 3600),
                (True, 0, 3600, "failure", 3600),
                (True, 890, 100000, "success", 4500),
            ):
                with self.subTest(role=role.__name__, enabled=enabled, assigned=assigned,
                                  finished=finished, conclusion=conclusion):
                    fixture = ApiFixture()
                    fixture.enabled = enabled
                    fixture.assign_at, fixture.finish_at = assigned, finished
                    fixture.run_conclusion = fixture.test_conclusion = conclusion
                    with real_api(fixture) as api:
                        result = role(api, REPO, fixture.src, clock=fixture.clock,
                                      sleep=fixture.clock.sleep)
                    expected = conclusion if finished <= expected_time else "hosted"
                    self.assertEqual(result, expected if role == dispatch.wait_for_mini else None)
                    self.assertEqual(fixture.clock(), expected_time)

    def test_final_observation_has_no_retry_or_unbounded_request_grace(self):
        fixture = ApiFixture()
        with real_api(fixture) as api:
            api.set_deadline(900, observe=True)
            fixture.clock.now = 900
            self.assertEqual(api.get(f"repos/{REPO}/actions/runs/1")["id"], 1)
            error = HTTPError("https://api.github.com/fixture", 429, "limited",
                              {"Retry-After": "1"}, None)
            with patch.object(dispatch.GitHubApi, "_request", side_effect=error) as request:
                with self.assertRaises(dispatch.Unavailable):
                    api.get("fixture")
                self.assertEqual(request.call_count, 1)
            fixture.clock.now = 960
            with self.assertRaises(dispatch.Unavailable):
                api.get(f"repos/{REPO}/actions/runs/1")
            api.set_deadline(960)  # Cleanup never gets observation grace.
            with self.assertRaises(dispatch.Unavailable):
                api.cancel_run(REPO, 10)

    def test_poll_crossing_queue_deadline_does_not_extend_execution(self):
        for role in (dispatch.wait_for_mini, dispatch.schedule):
            fixture = ApiFixture()
            fixture.assign_at, fixture.finish_at = 890, 100000
            original = fixture.collection

            def collection(path, key):
                if "/runs/10/" in path and fixture.clock() == 810:
                    fixture.clock.sleep(100)
                return original(path, key)

            fixture.collection = collection
            with real_api(fixture) as api:
                role(api, REPO, fixture.src, clock=fixture.clock, sleep=fixture.clock.sleep)
            self.assertEqual(fixture.clock(), 4500)

    def test_expired_gate_never_dispatches(self):
        fixture = ApiFixture()
        fixture.enabled = False
        fixture.gate_start_at = 900
        with real_api(fixture) as api:
            dispatch.schedule(api, REPO, fixture.src, clock=fixture.clock, sleep=fixture.clock.sleep)
        self.assertEqual(fixture.posts, [])

    def test_confirmed_failure_survives_unavailable_freshness_check(self):
        for conclusion in ("success", "failure"):
            fixture = ApiFixture()
            fixture.finish_at = 0
            fixture.run_conclusion = fixture.test_conclusion = conclusion
            original = fixture.collection

            def unavailable(path):
                raise URLError("offline")

            def collection(path, key):
                result = original(path, key)
                if "/runs/10/" in path:
                    fixture.get = unavailable
                return result

            fixture.collection = collection
            with real_api(fixture) as api:
                if conclusion == "failure":
                    self.assertEqual(dispatch.wait_for_mini(api, REPO, fixture.src,
                        clock=fixture.clock, sleep=fixture.clock.sleep), "failure")
                else:
                    with self.assertRaises(URLError):
                        dispatch.wait_for_mini(api, REPO, fixture.src,
                            clock=fixture.clock, sleep=fixture.clock.sleep)

    def test_obsolete_cleanup_failures_remain_independent(self):
        for failure in ("parent_read", "parent_cancel", "child_list", "child_cancel", None):
            fixture = ApiFixture()
            fixture.finish_at = 100000
            original_get, original_collection = fixture.get, fixture.collection
            original_cancel = fixture.cancel_run
            closed = False
            child_lists = 0

            def get(path):
                nonlocal closed
                if closed and failure == "parent_read" and path.endswith("/runs/1"):
                    raise URLError("parent lookup unavailable")
                result = original_get(path)
                if path.endswith("/pulls/1126") and fixture.clock() >= 120:
                    result["state"] = "closed"
                    closed = True
                return result

            def collection(path, key):
                nonlocal child_lists
                if closed and "/workflows/" in path:
                    child_lists += 1
                    if failure == "child_list":
                        raise URLError("child discovery unavailable")
                return original_collection(path, key)

            def cancel(repo, run_id):
                original_cancel(repo, run_id)
                if (run_id == 1 and failure == "parent_cancel"
                        or run_id == 10 and failure == "child_cancel"):
                    raise URLError("cancel unavailable")

            fixture.get, fixture.collection, fixture.cancel_run = get, collection, cancel
            with self.subTest(failure=failure), real_api(fixture) as api:
                status, _ = EntrypointTests().invoke(api, "schedule")
            self.assertEqual(status, 1 if failure else 0)
            self.assertGreater(child_lists, 0)
            self.assertEqual(fixture.cancelled.count(1), 0 if failure == "parent_read" else 1)
            self.assertEqual(fixture.cancelled.count(10), 0 if failure == "child_list" else 1)


class SchedulerTests(unittest.TestCase):
    def run_scheduler(self, api):
        return EntrypointTests().invoke(api, "schedule")[0]

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
            "source_run_id": "1", "source_attempt": "1", "source_sha": MERGE, "source_pool": "pr"}})

    def test_main_push_dispatches_only_the_dedicated_pool(self):
        api = ApiFixture()
        api.run.update(event="push", head_branch="main", head_repository={"id": 7},
            display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
        api.src = dispatch.Source(1, 1, HEAD)
        api.enabled = False
        api.assign_at = api.finish_at = 100000
        self.run_scheduler(api)
        self.assertEqual(api.posts[0][1]["inputs"], {
            "source_run_id": "1", "source_attempt": "1", "source_sha": HEAD, "source_pool": "main"})
        self.assertEqual(api.clock(), dispatch.QUEUE_SECONDS)
        self.assertEqual(api.cancelled, [10])

    def test_main_gate_owns_dispatch_and_completion_cleanup(self):
        for phase in ("skipped", "active", "completed"):
            with self.subTest(phase=phase):
                api = ApiFixture()
                api.run.update(event="push", head_branch="main", head_repository={"id": 7},
                    display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
                api.src = dispatch.Source(1, 1, HEAD)
                api.enabled = phase == "completed"
                if phase == "skipped":
                    api.gate_end_at = 0
                    api.gate_conclusion = "skipped"
                if phase == "completed":
                    api.run["status"] = "completed"
                    api.finish_at = 100000
                    other = api.worker()
                    other.update(id=20, display_title=dispatch.Source(2, 1, HEAD).title)
                    api.other_runs = [other]
                # The current repository switch is absent in this clean environment.
                # The active source gate, not a second switch read, owns dispatch;
                # completion must still clean up workers admitted before disablement.
                mode = "cleanup" if phase == "completed" else "schedule"
                self.assertEqual(EntrypointTests().invoke(api, mode, sha=HEAD)[0], 0)
                if phase == "active":
                    self.assertEqual(api.posts[0][1]["inputs"], {
                        "source_run_id": "1", "source_attempt": "1",
                        "source_sha": HEAD, "source_pool": "main"})
                else:
                    self.assertEqual(api.posts, [])
                self.assertEqual(api.cancelled, [10] if phase == "completed" else [])

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
        self.assertEqual(api.dispatch_deadline, api.dispatch_time + dispatch.QUEUE_SECONDS)

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
            self.assertEqual(self.run_scheduler(api), 0)
            self.assertEqual(api.cancelled, [10])
            self.assertLessEqual(api.clock(), 20 + dispatch.RUNNING_POLL_SECONDS)

    def test_success_or_real_test_failure_does_not_cancel_completed_worker(self):
        for result in ("success", "failure"):
            api = ApiFixture()
            api.run_conclusion = api.test_conclusion = result
            self.run_scheduler(api)
            self.assertEqual(api.cancelled, [])


class EntrypointTests(unittest.TestCase):
    def invoke(self, api, mode, *, worker_attempt="1", pool="pr", sha=MERGE):
        schedule = dispatch.schedule
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(dispatch.time, "monotonic", side_effect=api.clock), \
             patch.object(dispatch, "schedule", side_effect=lambda a, r, s, **kw:
                 schedule(a, r, s, clock=api.clock, sleep=api.clock.sleep, **kw)):
            output = Path(directory) / "output"
            args = ["macos_dispatch.py", mode, "--run-id", "1", "--attempt", "1", "--sha", sha]
            with patch.object(dispatch, "Api", return_value=api), patch.object(sys, "argv", args), \
                 patch.dict(os.environ, {"GITHUB_REPOSITORY": REPO, "GH_TOKEN": "fixture",
                     "GITHUB_OUTPUT": str(output), "GITHUB_RUN_ATTEMPT": worker_attempt, "SOURCE_POOL": pool}, clear=True):
                status = dispatch.main()
            return status, output.read_text() if output.exists() else ""

    def test_pool_admission_binds_the_verified_source(self):
        pr = ApiFixture()
        self.assertEqual(self.invoke(pr, "admit", pool="pr")[0], 0)
        self.assertEqual(self.invoke(pr, "admit", pool="main")[0], 1)
        main = ApiFixture()
        main.run.update(event="push", head_branch="main", head_repository={"id": 7},
            display_title=f"Test Suite pr=0 base={BASE} head={HEAD} merge={HEAD}")
        self.assertEqual(self.invoke(main, "admit", pool="main", sha=HEAD)[0], 0)
        self.assertEqual(self.invoke(main, "admit", pool="pr", sha=HEAD)[0], 1)
        self.assertEqual(self.invoke(main, "admit", pool="main", sha=MERGE)[0], 1)

    def test_unavailable_api_and_missing_controller_fail_open_to_actual_hosted_tests(self):
        api = ApiFixture()
        with patch.object(api, "get", side_effect=RuntimeError("unavailable")):
            self.assertEqual(self.invoke(api, "wait"), (0, "result=hosted\n"))

    def test_failure_is_published_as_failure_not_fallback(self):
        api = ApiFixture()
        api.finish_at = 0
        api.run_conclusion = api.test_conclusion = "failure"
        self.assertEqual(self.invoke(api, "wait"), (0, "result=failure\n"))

    def test_updated_or_closed_pr_does_not_start_hosted_tests(self):
        for change in ({"state": "closed"}, {"state": "closed", "head": {"sha": HEAD, "repo": None}},
                       {"head": {"sha": "d" * 40, "repo": {"id": 42}}}):
            api = ApiFixture()
            api.pr.update(change)
            self.assertEqual(self.invoke(api, "wait"), (0, "result=obsolete\n"))
            self.assertEqual(self.invoke(api, "schedule")[0], 0)
            self.assertIn(1, api.cancelled)
            self.assertIn(10, api.cancelled)
            self.assertEqual(self.invoke(api, "admit")[0], 1)
            self.assertEqual(api.posts, [])

    def test_unknown_pr_state_keeps_hosted_recovery_instead_of_cancelling(self):
        api = ApiFixture()
        api.pr.pop("state")
        self.assertEqual(self.invoke(api, "wait"), (0, "result=hosted\n"))
        self.assertEqual(api.cancelled, [])

    def test_obsolete_controller_never_cancels_a_new_attempt(self):
        api = ApiFixture()
        original = api.get
        calls = 0

        def get(path):
            nonlocal calls
            if path.endswith("/actions/runs/1"):
                calls += 1
                if calls == 2:
                    api.run["run_attempt"] = 2
            return original(path)

        api.pr["state"] = "closed"
        api.get = get
        self.assertEqual(self.invoke(api, "schedule")[0], 0)
        self.assertNotIn(1, api.cancelled)
        self.assertIn(10, api.cancelled)

    def test_obsolete_parent_cancel_failure_still_cleans_up_child(self):
        api = ApiFixture()
        api.pr["state"] = "closed"
        original = api.cancel_run

        def cancel(repo, run_id):
            if run_id == 1:
                raise URLError("temporary")
            return original(repo, run_id)

        api.cancel_run = cancel
        self.assertEqual(self.invoke(api, "schedule")[0], 1)
        self.assertIn(10, api.cancelled)
        self.assertEqual(self.invoke(api, "wait"), (0, "result=obsolete\n"))

    def test_scheduler_and_parent_timeouts_cover_queue_plus_execution(self):
        import re
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        gate = workflow.split("\n  macos-plan:\n")[1].split("    steps:\n")[0]
        scheduler = (ROOT / ".github/workflows/macos-scheduler.yml").read_text()
        self.assertGreater(int(re.search(r"timeout-minutes: (\d+)", gate)[1]) * 60,
                           dispatch.QUEUE_SECONDS + dispatch.EXECUTION_SECONDS)
        self.assertGreater(int(re.search(r"timeout-minutes: (\d+)", scheduler)[1]) * 60,
                           2 * dispatch.QUEUE_SECONDS + dispatch.EXECUTION_SECONDS)
        # Assert the actual workflow contract without emulating Actions' evaluator.
        condition = re.search(r"    if: >-\n(.*?)\n    runs-on:", scheduler, re.S)[1]
        self.assertEqual(" ".join(condition.split()),
            "github.event.workflow_run.run_attempt == 1 && "
            "(github.event.workflow_run.event == 'pull_request' || "
            "(github.event.workflow_run.event == 'push' && "
            "github.event.workflow_run.head_branch == 'main'))")

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
