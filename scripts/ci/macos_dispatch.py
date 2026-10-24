#!/usr/bin/env python3
"""Bounded mini attempt; ordinary Test Suite owns hosted fallback and checks."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import time
from urllib.parse import urlencode

from cancel_stale_pr_runs import GitHubApi, MAX_PAGES
from ci_scope import MACOS_SELF_HOSTED_LABELS, select_macos_runner


WORKFLOW = "macos-mini.yml"
GATE = "Resolve macOS execution"
WAIT_STEP = "Wait for a bounded mini attempt"
TEST_STEP = "Run all macOS lanes"
PREFLIGHT_STEPS = {"Set up job", "Checkout control policy", "Validate mini dispatch",
                   "Checkout candidate", "Prepare Rust", "Prepare Node"}
QUEUE_SECONDS = 180
EXECUTION_SECONDS = 3600
POLL_SECONDS = 20
RUNNING_POLL_SECONDS = 60
TITLE = re.compile(r"Test Suite pr=(\d+) base=([0-9a-f]{40}) head=([0-9a-f]{40}) merge=([0-9a-f]{40})")


class Unavailable(RuntimeError):
    pass


class Api(GitHubApi):
    def get(self, path):
        return self._get_json(path)

    def post(self, path, data):
        self._request("POST", path, data=data)

    def collection(self, path, key):
        result, visited = [], set()
        while path:
            if path in visited or len(visited) >= MAX_PAGES:
                raise Unavailable("pagination bound exceeded")
            visited.add(path)
            body, headers = self._request("GET", path)
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

    @property
    def title(self):
        return f"Mac mini for Test Suite {self.run_id}/{self.attempt} {self.sha}"


def source(api, repo, run_id, attempt):
    run = api.get(f"repos/{repo}/actions/runs/{run_id}")
    match = TITLE.fullmatch(run.get("display_title", ""))
    # GitHub may populate name with the dynamic run-name as well. The workflow
    # path is the identity; the title carries the immutable event tuple.
    if (run.get("id") != run_id or run.get("path") != ".github/workflows/test.yml"
            or run.get("run_attempt") != attempt or attempt != 1
            or run.get("status") == "completed" or not match):
        raise Unavailable("source run is not a current mini candidate")
    pr_number, base, head, merge = match.groups()
    if run.get("head_sha") != head:
        raise Unavailable("source head mismatch")
    if run.get("event") == "pull_request" and int(pr_number) > 0:
        # Fork run.pull_requests can be empty. The event title records the PR
        # and immutable merge tuple, rather than relying on that association.
        pr = api.get(f"repos/{repo}/pulls/{pr_number}")
        if (pr["head"]["sha"] != head or pr["state"] != "open"
                or pr["head"]["repo"]["id"] != run["head_repository"]["id"]):
            raise Unavailable("PR source changed")
        parents = api.get(f"repos/{repo}/commits/{merge}")["parents"]
        if [parent["sha"] for parent in parents] != [base, head]:
            raise Unavailable("unverified merge tuple")
        labels = select_macos_runner("pull_request", {"pull_request": {"user": pr["user"]}})
    else:
        raise Unavailable("unsupported source event")
    if labels != MACOS_SELF_HOSTED_LABELS:
        raise Unavailable("source does not prefer mini")
    return Source(run_id, attempt, merge, run.get("created_at"))


def jobs(api, repo, run_id, attempt):
    return api.collection(
        f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100", "jobs")


def gate_active(api, repo, src):
    run = api.get(f"repos/{repo}/actions/runs/{src.run_id}")
    if run.get("run_attempt") != src.attempt or run.get("status") == "completed":
        return False
    return any(job.get("name") == GATE and job.get("status") == "in_progress"
               and any(step.get("name") == WAIT_STEP and step.get("status") == "in_progress"
                       for step in job.get("steps", []))
               for job in jobs(api, repo, src.run_id, src.attempt))


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


def wait_for_mini(api, repo, src, *, clock=time.monotonic, sleep=time.sleep):
    queued_at, assigned_at = clock(), None
    announced = None
    while True:
        run = api.get(f"repos/{repo}/actions/runs/{src.run_id}")
        if run.get("run_attempt") != src.attempt or run.get("status") == "completed":
            raise Unavailable("source attempt ended")
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
                return outcome
            if assigned and assigned_at is None:
                assigned_at = clock()
        if assigned_at is None and clock() - queued_at >= QUEUE_SECONDS:
            print("::notice::Mini queue/discovery budget expired; using hosted macOS")
            return "hosted"
        if assigned_at is not None and clock() - assigned_at >= EXECUTION_SECONDS:
            print("::notice::Mini execution budget expired; using hosted macOS")
            return "hosted"
        sleep(RUNNING_POLL_SECONDS if assigned_at is not None else POLL_SECONDS)


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


def schedule(api, repo, src, *, clock=time.monotonic, sleep=time.sleep):
    queued_at, assigned_at = clock(), None
    try:
        # No worker is dispatched unless the approved ordinary workflow has
        # actually started its gate. A delayed scheduler cannot resurrect it.
        while not gate_active(api, repo, src):
            if any(job.get("name") == GATE and (job.get("status") == "completed"
                   or any(step.get("name") == WAIT_STEP and step.get("status") == "completed"
                          for step in job.get("steps", [])))
                   for job in jobs(api, repo, src.run_id, src.attempt)):
                return
            run = api.get(f"repos/{repo}/actions/runs/{src.run_id}")
            if run.get("status") == "completed" or run.get("run_attempt") != src.attempt:
                return
            if clock() - queued_at >= QUEUE_SECONDS:
                return
            sleep(POLL_SECONDS)
        queued_at = clock()
        if not workers(api, repo, src):
            api.post(f"repos/{repo}/actions/workflows/{WORKFLOW}/dispatches", {
                "ref": "main", "inputs": {"source_run_id": str(src.run_id),
                    "source_attempt": str(src.attempt), "source_sha": src.sha}})
        while gate_active(api, repo, src):
            candidates = workers(api, repo, src)
            if len(candidates) > 1:
                raise Unavailable("duplicate mini dispatch")
            if candidates:
                outcome, assigned = worker_state(api, repo, candidates[0])
                if outcome:
                    return
                if assigned and assigned_at is None:
                    assigned_at = clock()
            if assigned_at is None and clock() - queued_at >= QUEUE_SECONDS:
                return
            if assigned_at is not None and clock() - assigned_at >= EXECUTION_SECONDS:
                return
            sleep(RUNNING_POLL_SECONDS if assigned_at is not None else POLL_SECONDS)
    finally:
        # Also runs when the parent is cancelled/superseded or has already
        # selected hosted fallback. Only this exact source/attempt is touched.
        cancel_workers(api, repo, src)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("schedule", "wait", "admit", "cleanup"))
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--sha")
    args = parser.parse_args()
    repo = os.environ["GITHUB_REPOSITORY"]
    api = Api(os.environ.get("GH_TOKEN", ""), os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    outcome = "hosted"
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
            schedule(api, repo, src)
            return 0
        if args.mode == "admit":
            if os.environ.get("GITHUB_RUN_ATTEMPT") != "1":
                raise Unavailable("worker reruns must use ordinary hosted recovery")
            if src.sha != args.sha or not gate_active(api, repo, src):
                raise Unavailable("dispatch expired before execution")
            return 0
        outcome = wait_for_mini(api, repo, src)
    except Exception as error:
        # Do not print API bodies, credential-bearing URLs or private config.
        reason = str(error) if isinstance(error, Unavailable) else type(error).__name__
        if isinstance(getattr(error, "code", None), int):
            reason += f" HTTP {error.code}"
        print(f"::notice::Mini unavailable ({reason}); using hosted macOS")
        if args.mode == "admit":
            return 1
    if args.mode == "wait":
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write(f"result={outcome}\n")
        print(f"macOS execution result: {outcome}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
