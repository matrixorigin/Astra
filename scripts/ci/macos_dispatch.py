#!/usr/bin/env python3
"""Bounded mini attempt; ordinary Test Suite owns hosted fallback and checks."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
from http.client import IncompleteRead
import json
import os
from pathlib import Path
import re
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode

from cancel_stale_pr_runs import GitHubApi, MAX_PAGES
from ci_scope import MACOS_MAIN_LABELS, MACOS_SELF_HOSTED_LABELS, select_macos_runner


WORKFLOW = "macos-mini.yml"
GATE = "Resolve macOS execution"
WAIT_STEP = "Wait for a bounded mini attempt"
TEST_STEP = "Run all macOS lanes"
PREFLIGHT_STEPS = {"Set up job", "Checkout control policy", "Validate mini dispatch",
                   "Checkout candidate", "Prepare Rust", "Prepare Node"}
QUEUE_SECONDS = 900
EXECUTION_SECONDS = 3600
POLL_SECONDS = 90
RUNNING_POLL_SECONDS = 120
OBSERVATION_SECONDS = 60
TITLE = re.compile(r"Test Suite pr=(\d+) base=([0-9a-f]{40}) head=([0-9a-f]{40}) merge=([0-9a-f]{40})")


class Unavailable(RuntimeError):
    pass


class Superseded(Unavailable):
    """An obsolete source needs cancellation, not another executor."""

    def __init__(self, reason, src):
        super().__init__(reason)
        self.src = src


class Api(GitHubApi):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._conditional = {}
        self._cooldown = 0
        self._deadline = None
        self._request_deadline = None
        self._quota_reported = False

    def set_deadline(self, deadline, *, observe=False):
        self._deadline = deadline
        self._request_deadline = deadline + (OBSERVATION_SECONDS if observe else 0)

    def _pause(self):
        now = time.monotonic()
        delay = max(0, self._cooldown - now)
        if self._request_deadline is not None and now >= self._request_deadline:
            raise Unavailable("mini API request budget expired")
        if delay and self._deadline is not None and now + delay >= self._deadline:
            raise Unavailable("API backoff exceeds the remaining mini budget")
        if delay:
            time.sleep(delay)

    def _report_quota(self, headers):
        # Read the actual workflow token's quota, not a developer PAT's quota.
        # Print only explicitly selected numeric headers, never the raw response.
        keys = ("X-RateLimit-Limit", "X-RateLimit-Remaining", "X-RateLimit-Reset")
        values = [headers.get(key, "") for key in keys]
        if not self._quota_reported and all(str(value).isdigit() for value in values):
            self._quota_reported = True
            message = "GitHub API quota: limit={}, remaining={}, reset={}".format(*values)
            print(f"::notice::{message}", flush=True)
            if os.environ.get("GITHUB_STEP_SUMMARY"):
                try:
                    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
                        summary.write(message + "\n")
                except OSError:
                    print("::warning::Could not write quota summary; numeric quota remains in the log")

    @staticmethod
    def _limit_delay(error):
        if error.code not in (403, 429):
            return None
        headers = error.headers or {}
        delays = []
        if headers.get("X-RateLimit-Remaining") == "0":
            try:
                delays.append(max(1, float(headers["X-RateLimit-Reset"]) - time.time() + 1))
            except (KeyError, ValueError, TypeError):
                delays.append(60)
        retry_after = headers.get("Retry-After")
        if retry_after is not None:
            try:
                delays.append(max(1, float(retry_after)))
            except (TypeError, ValueError):
                try:
                    delays.append(max(1, parsedate_to_datetime(retry_after).timestamp() - time.time() + 1))
                except (TypeError, ValueError, OverflowError):
                    delays.append(60)
        if delays:
            return max(delays)
        if error.code == 429:
            return 60
        # Permission-denied 403s must fail immediately. A secondary-limit 403
        # may have no headers; inspect a bounded message without logging it.
        try:
            message = json.loads(error.read(4096)).get("message", "").lower()
        except (ValueError, AttributeError, OSError, IncompleteRead):
            message = ""
        return 60 if "rate limit" in message or "abuse detection" in message else None

    def _request(self, method, url, *, data=None, etag=None):
        self._pause()
        try:
            if etag is None:
                response = super()._request(method, url, data=data)
            else:
                response = super()._request(method, url, data=data, etag=etag)
        except HTTPError as error:
            self._report_quota(error.headers or {})
            error.mini_limit_delay = self._limit_delay(error)
            if error.mini_limit_delay is not None:
                self._cooldown = time.monotonic() + error.mini_limit_delay
            raise
        self._report_quota(response[1])
        return response

    def _read(self, path):
        # Retry only idempotent reads. Retrying dispatch could create duplicate
        # workers; authorization failures must still fall back immediately.
        for attempt in range(3):
            try:
                cached = self._conditional.get(path)
                response = (self._request("GET", path, etag=cached[1]["ETag"])
                            if cached else self._request("GET", path))
                if response[1].get("ETag"):
                    self._conditional[path] = response
                else:
                    self._conditional.pop(path, None)
                return response
            except (URLError, TimeoutError, ConnectionError, IncompleteRead) as error:
                delay = None
                if isinstance(error, HTTPError):
                    if error.code == 304:
                        error.close()
                        if not cached:
                            raise Unavailable("conditional response has no verified cached body")
                        return cached
                    delay = getattr(error, "mini_limit_delay", None)
                    error.close()
                if (isinstance(error, HTTPError) and error.code < 500 and delay is None) or attempt == 2:
                    raise
                if delay is None:
                    delay = 2 * (attempt + 1)
                elif not (error.headers or {}).get("Retry-After") and (error.headers or {}).get("X-RateLimit-Remaining") != "0":
                    delay *= 2 ** attempt
                self._cooldown = time.monotonic() + delay
                print("::notice::Backing off before retrying a mini API read")
                self._pause()

    def get(self, path):
        return json.loads(self._read(path)[0])

    def post(self, path, data):
        self._request("POST", path, data=data)

    def collection(self, path, key):
        result, visited = [], set()
        while path:
            if path in visited or len(visited) >= MAX_PAGES:
                raise Unavailable("pagination bound exceeded")
            visited.add(path)
            body, headers = self._read(path)
            page = json.loads(body)
            if not isinstance(page.get(key), list):
                raise Unavailable("missing API collection")
            result.extend(page[key])
            path = self._next_link(headers)
        return result


@dataclass(frozen=True)
class Source:
    run_id: int
    attempt: int
    sha: str
    created_at: str | None = None
    pr_number: int = 0
    head_sha: str = ""
    head_repo_id: int | None = None

    @property
    def title(self):
        return f"Mac mini for Test Suite {self.run_id}/{self.attempt} {self.sha}"


def check_current(api, repo, src, run=None):
    run = run if run is not None else api.get(f"repos/{repo}/actions/runs/{src.run_id}")
    if run.get("run_attempt") != src.attempt or run.get("status") == "completed":
        raise Superseded("source attempt ended", src)
    if src.pr_number:
        pr = api.get(f"repos/{repo}/pulls/{src.pr_number}")
        if pr.get("state") == "closed":
            raise Superseded("PR was closed", src)
        if pr.get("state") != "open":
            raise Unavailable("unknown PR state")
        if (pr.get("head", {}).get("repo") or {}).get("id") != src.head_repo_id:
            raise Unavailable("PR head repository changed")
        if pr["head"]["sha"] != src.head_sha:
            raise Superseded("PR was updated", src)
        return pr
    return None


def source(api, repo, run_id, attempt):
    run = api.get(f"repos/{repo}/actions/runs/{run_id}")
    match = TITLE.fullmatch(run.get("display_title", ""))
    # GitHub may populate name with the dynamic run-name as well. The workflow
    # path is the identity; the title carries the immutable event tuple.
    if (run.get("id") != run_id or run.get("path") != ".github/workflows/test.yml"
            or run.get("run_attempt") != attempt or attempt != 1 or not match):
        raise Unavailable("source run is not a current mini candidate")
    pr_number, event_base, head, merge = match.groups()
    if run.get("head_sha") != head:
        raise Unavailable("source head mismatch")
    if run.get("event") == "pull_request" and int(pr_number) > 0:
        # Fork run.pull_requests can be empty. The event title records the PR
        # and immutable merge tuple, rather than relying on that association.
        src = Source(run_id, attempt, merge, run.get("created_at"),
                     int(pr_number), head, run["head_repository"]["id"])
        pr = check_current(api, repo, src, run)
        parents = api.get(f"repos/{repo}/commits/{merge}")["parents"]
        if len(parents) != 2 or parents[1]["sha"] != head:
            raise Unavailable("unverified merge tuple")
        # The PR event's base SHA can lag the actual synthetic merge. Validate
        # its real first parent against the target branch, not that stale field.
        if pr["base"]["repo"]["id"] != run["repository"]["id"]:
            raise Unavailable("unexpected PR base repository")
        target = api.get(f"repos/{repo}/git/ref/heads/{quote(pr['base']['ref'], safe='')}")["object"]["sha"]
        actual_base = parents[0]["sha"]
        if actual_base != target:
            comparison = api.get(f"repos/{repo}/compare/{actual_base}...{target}")
            if comparison.get("status") not in {"ahead", "identical"}:
                raise Unavailable("merge base is not on the target branch")
        if actual_base != event_base:
            print("::notice::Validated actual merge parents despite an outdated event base SHA")
        labels = select_macos_runner("pull_request", {"pull_request": {"user": pr["user"]}})
    elif (run.get("event") == "push" and int(pr_number) == 0
          and run.get("head_branch") == "main" and merge == head
          and run.get("head_repository", {}).get("id") == run.get("repository", {}).get("id")
          and run.get("repository", {}).get("id") is not None):
        src = Source(run_id, attempt, head, run.get("created_at"))
        check_current(api, repo, src, run)
        labels = MACOS_MAIN_LABELS
    else:
        raise Unavailable("unsupported source event")
    if labels not in (MACOS_SELF_HOSTED_LABELS, MACOS_MAIN_LABELS):
        raise Unavailable("source does not prefer mini")
    return src


def jobs(api, repo, run_id, attempt):
    return api.collection(
        f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100", "jobs")


def gate_status(api, repo, src):
    run = api.get(f"repos/{repo}/actions/runs/{src.run_id}")
    check_current(api, repo, src, run)
    for job in jobs(api, repo, src.run_id, src.attempt):
        if job.get("name") != GATE:
            continue
        steps = [step for step in job.get("steps", []) if step.get("name") == WAIT_STEP]
        if job.get("status") == "completed" or any(step.get("status") == "completed" for step in steps):
            return "ended"
        if job.get("status") == "in_progress" and any(step.get("status") == "in_progress" for step in steps):
            return "active"
    return "waiting"


def gate_active(api, repo, src):
    return gate_status(api, repo, src) == "active"


def workers(api, repo, src):
    filters = {"event": "workflow_dispatch", "branch": "main", "per_page": 100}
    if src.created_at:
        filters["created"] = ">=" + src.created_at
    query = urlencode(filters)
    runs = api.collection(f"repos/{repo}/actions/workflows/{WORKFLOW}/runs?{query}", "workflow_runs")
    return [run for run in runs if run.get("display_title") == src.title
            and run.get("path") == f".github/workflows/{WORKFLOW}"
            and run.get("event") == "workflow_dispatch"]


def worker_state(api, repo, run):
    if run.get("run_attempt") != 1:
        raise Unavailable("worker reruns are not reusable")
    execution = jobs(api, repo, run["id"], 1)
    if len(execution) > 1:
        raise Unavailable("unexpected mini job inventory")
    job = execution[0] if execution else {}
    assigned = bool(job.get("runner_id")) and job.get("status") == "in_progress"
    tests = [step for step in job.get("steps", []) if step.get("name") == TEST_STEP]
    # A known test failure wins even while post-job cleanup is still running.
    if any(step.get("conclusion") == "failure" for step in tests):
        return "failure", True
    if run.get("status") != "completed":
        return None, assigned
    if len(tests) == 1:
        if run.get("conclusion") == "success" and tests[0].get("conclusion") == "success":
            return "success", True
    preflight_failed = any(step.get("name") in PREFLIGHT_STEPS and step.get("conclusion") == "failure"
                           for step in job.get("steps", []))
    if (run.get("conclusion") == "failure" and not preflight_failed
            and not (len(tests) == 1 and tests[0].get("conclusion") == "success")):
        # Incomplete/eventually consistent job metadata must not turn a failed
        # test run into a successful hosted retry. Only proven setup failures
        # or an explicit infrastructure terminal state get that fallback.
        return "failure", assigned
    # Checkout, admission, low disk, toolchain, agent loss or an execution
    # deadline are infrastructure outcomes. Never infer success from a skip.
    print("::notice::Mini ended without a completed passing test step; using hosted macOS")
    return "hosted", assigned


def wait_for_mini(api, repo, src, *, clock=time.monotonic, sleep=time.sleep, queued_at=None):
    queued_at, assigned_at = clock() if queued_at is None else queued_at, None
    announced = None
    while True:
        deadline = (assigned_at + EXECUTION_SECONDS if assigned_at is not None else queued_at + QUEUE_SECONDS)
        api.set_deadline(deadline, observe=True)
        check_current(api, repo, src)
        candidates = workers(api, repo, src)
        if len(candidates) > 1:
            raise Unavailable("duplicate mini dispatch")
        if candidates:
            if candidates[0]["id"] != announced:
                announced = candidates[0]["id"]
                print(f"Mini worker run: {announced}")
                if os.environ.get("GITHUB_STEP_SUMMARY"):
                    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
                        summary.write(f"Mini worker: https://github.com/{repo}/actions/runs/{announced}\n")
            outcome, assigned = worker_state(api, repo, candidates[0])
            if outcome:
                # A throttled read may have waited while the PR changed.
                try:
                    check_current(api, repo, src)
                except Superseded:
                    raise
                except Exception:
                    if outcome != "failure":
                        raise
                    print("::warning::Source revalidation unavailable; retaining the test failure")
                return outcome
            if assigned and assigned_at is None:
                assigned_at = min(clock(), queued_at + QUEUE_SECONDS)
        if assigned_at is None and clock() - queued_at >= QUEUE_SECONDS:
            print("::notice::Mini queue/discovery budget expired; using hosted macOS")
            return "hosted"
        if assigned_at is not None and clock() - assigned_at >= EXECUTION_SECONDS:
            print("::notice::Mini execution budget expired; using hosted macOS")
            return "hosted"
        deadline = (assigned_at + EXECUTION_SECONDS if assigned_at is not None else queued_at + QUEUE_SECONDS)
        sleep(min(RUNNING_POLL_SECONDS if assigned_at is not None else POLL_SECONDS,
                  max(0, deadline - clock())))


def cancel_workers(api, repo, src):
    failures = 0
    for run in workers(api, repo, src):
        if run.get("status") != "completed":
            try:
                api.cancel_run(repo, run["id"])
            except Exception:
                # A transient failure for one duplicate must not prevent
                # cleanup of the other workers belonging to this source.
                failures += 1
    if failures:
        raise Unavailable(f"cancellation failed for {failures} worker(s)")


def schedule(api, repo, src, *, clock=time.monotonic, sleep=time.sleep, queued_at=None):
    queued_at, assigned_at = clock() if queued_at is None else queued_at, None
    api.set_deadline(queued_at + QUEUE_SECONDS, observe=True)
    # No worker is dispatched unless the approved ordinary workflow has
    # actually started its gate. A delayed scheduler cannot resurrect it.
    while True:
        state = gate_status(api, repo, src)
        if state == "ended":
            return
        if clock() - queued_at >= QUEUE_SECONDS:
            return
        if state == "active":
            break
        sleep(min(POLL_SECONDS, max(0, queued_at + QUEUE_SECONDS - clock())))
    queued_at = clock()
    api.set_deadline(queued_at + QUEUE_SECONDS)
    if not workers(api, repo, src):
        api.post(f"repos/{repo}/actions/workflows/{WORKFLOW}/dispatches", {
            "ref": "main", "inputs": {"source_run_id": str(src.run_id),
                "source_attempt": str(src.attempt), "source_sha": src.sha,
                "source_pool": "pr" if src.pr_number else "main"}})
    while True:
        deadline = (assigned_at + EXECUTION_SECONDS if assigned_at is not None else queued_at + QUEUE_SECONDS)
        api.set_deadline(deadline, observe=True)
        if not gate_active(api, repo, src):
            return
        candidates = workers(api, repo, src)
        if len(candidates) > 1:
            raise Unavailable("duplicate mini dispatch")
        if candidates:
            outcome, assigned = worker_state(api, repo, candidates[0])
            if outcome:
                return
            if assigned and assigned_at is None:
                assigned_at = min(clock(), queued_at + QUEUE_SECONDS)
        if assigned_at is None and clock() - queued_at >= QUEUE_SECONDS:
            return
        if assigned_at is not None and clock() - assigned_at >= EXECUTION_SECONDS:
            return
        deadline = (assigned_at + EXECUTION_SECONDS if assigned_at is not None else queued_at + QUEUE_SECONDS)
        sleep(min(RUNNING_POLL_SECONDS if assigned_at is not None else POLL_SECONDS,
                  max(0, deadline - clock())))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("schedule", "wait", "admit", "cleanup"))
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--sha")
    args = parser.parse_args()
    repo = os.environ["GITHUB_REPOSITORY"]
    api = Api(os.environ.get("GH_TOKEN", ""), os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    queued_at = time.monotonic()
    api.set_deadline(queued_at + (QUEUE_SECONDS if args.mode in {"wait", "schedule"} else 120))
    outcome, status, src = "hosted", 0, None
    try:
        if args.mode == "cleanup":
            run = api.get(f"repos/{repo}/actions/runs/{args.run_id}")
            match = TITLE.fullmatch(run.get("display_title", ""))
            if run.get("path") == ".github/workflows/test.yml" and match:
                # Completion also repairs an orphan left by a scheduler/API
                # failure; the PR may already be closed or on a newer attempt.
                cancel_workers(api, repo, Source(args.run_id, args.attempt, match[4], run.get("created_at")))
            return 0
        src = source(api, repo, args.run_id, args.attempt)
        if args.mode == "schedule":
            schedule(api, repo, src, queued_at=queued_at)
        elif args.mode == "admit":
            if os.environ.get("GITHUB_RUN_ATTEMPT") != "1":
                raise Unavailable("worker reruns must use ordinary hosted recovery")
            if (src.sha != args.sha or os.environ.get("SOURCE_POOL") != ("pr" if src.pr_number else "main")
                    or not gate_active(api, repo, src)):
                raise Unavailable("dispatch expired before execution")
            return 0
        else:
            outcome = wait_for_mini(api, repo, src, queued_at=queued_at)
    except Superseded as error:
        print(f"::notice::Obsolete mini source ({error}); no hosted retry")
        outcome = "obsolete"
        src = error.src
        if args.mode == "admit":
            return 1
    except Exception as error:
        # Do not print API bodies, credential-bearing URLs or private config.
        reason = str(error) if isinstance(error, Unavailable) else type(error).__name__
        if isinstance(getattr(error, "code", None), int):
            reason += f" HTTP {error.code}"
        print(f"::notice::Mini unavailable ({reason})")
        if args.mode in {"admit", "cleanup"}:
            return 1
        if args.mode == "schedule":
            status = 1
    finally:
        if args.mode == "schedule" and src is not None:
            # One owner for cleanup, including obsolescence during admission.
            # Keep the source outcome; parent and child failures are independent.
            api.set_deadline(time.monotonic() + 120)
            if outcome == "obsolete":
                try:
                    run = api.get(f"repos/{repo}/actions/runs/{src.run_id}")
                    if run.get("run_attempt") == src.attempt and run.get("status") != "completed":
                        api.cancel_run(repo, src.run_id)
                except Exception:
                    print("::warning::Obsolete parent cancellation incomplete")
                    status = 1
            try:
                cancel_workers(api, repo, src)
            except Exception:
                print("::warning::Mini worker cleanup incomplete")
                status = 1
    if args.mode == "wait":
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write(f"result={outcome}\n")
        print(f"macOS execution result: {outcome}")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
