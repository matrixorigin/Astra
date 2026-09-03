#!/usr/bin/env python3
"""Cancel active GitHub Actions runs that belong to an earlier head of a PR."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
import re
import sys
from typing import Any, Protocol
from urllib.error import HTTPError
from urllib.parse import urlencode, urljoin, urlsplit
from urllib.request import Request, urlopen


ACTIVE_RUN_STATUSES = ("queued", "in_progress", "pending", "requested", "waiting")
MAX_PAGES_PER_STATUS = 10
MAX_STALE_RUNS = 1_000
REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")


@dataclass(frozen=True)
class PullRequestHead:
    repository_id: int
    repository: str
    ref: str
    sha: str


class ActionsApi(Protocol):
    def get_pull_request(self, repository: str, number: int) -> dict[str, Any]: ...

    def list_runs(self, repository: str, status: str) -> list[dict[str, Any]]: ...

    def cancel_run(self, repository: str, run_id: int) -> bool: ...


class GitHubApi:
    """Small GitHub REST client with bounded, origin-checked pagination."""

    def __init__(self, token: str, api_url: str = "https://api.github.com") -> None:
        if not token:
            raise ValueError("GH_TOKEN is required")
        self._token = token
        self._api_url = api_url.rstrip("/") + "/"
        self._api_origin = urlsplit(self._api_url).netloc

    def _request(self, method: str, url: str) -> tuple[bytes, Any]:
        target = url if urlsplit(url).scheme else urljoin(self._api_url, url.lstrip("/"))
        if urlsplit(target).netloc != self._api_origin:
            raise RuntimeError("refusing to send the GitHub token to another origin")
        request = Request(
            target,
            method=method,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self._token}",
                "User-Agent": "astra-stale-pr-run-controller",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urlopen(request, timeout=30) as response:
            return response.read(), response.headers

    def _get_json(self, path: str) -> dict[str, Any]:
        body, _ = self._request("GET", path)
        value = json.loads(body)
        if not isinstance(value, dict):
            raise RuntimeError(f"GitHub returned a non-object response for {path}")
        return value

    @staticmethod
    def _next_link(headers: Any) -> str | None:
        for part in headers.get("Link", "").split(","):
            match = re.match(r'\s*<([^>]+)>;\s*rel="([^"]+)"\s*$', part)
            if match and match.group(2) == "next":
                return match.group(1)
        return None

    def get_pull_request(self, repository: str, number: int) -> dict[str, Any]:
        return self._get_json(f"repos/{repository}/pulls/{number}")

    def list_runs(self, repository: str, status: str) -> list[dict[str, Any]]:
        query = urlencode({"event": "pull_request", "status": status, "per_page": 100})
        next_url: str | None = f"repos/{repository}/actions/runs?{query}"
        runs: list[dict[str, Any]] = []
        visited: set[str] = set()
        while next_url:
            if next_url in visited or len(visited) >= MAX_PAGES_PER_STATUS:
                raise RuntimeError(f"workflow-run pagination exceeded its safe bound for {status}")
            visited.add(next_url)
            body, headers = self._request("GET", next_url)
            page = json.loads(body)
            page_runs = page.get("workflow_runs") if isinstance(page, dict) else None
            if not isinstance(page_runs, list):
                raise RuntimeError("GitHub workflow-runs response omitted workflow_runs")
            runs.extend(run for run in page_runs if isinstance(run, dict))
            next_url = self._next_link(headers)
        return runs

    def cancel_run(self, repository: str, run_id: int) -> bool:
        try:
            self._request("POST", f"repos/{repository}/actions/runs/{run_id}/cancel")
        except HTTPError as error:
            # A selected run can finish between the list and cancel requests.
            if error.code == 409:
                return False
            raise
        return True


def _pull_request_head(pull_request: dict[str, Any]) -> PullRequestHead:
    head = pull_request.get("head")
    if not isinstance(head, dict):
        raise RuntimeError("pull request response omitted head")
    repository = head.get("repo")
    full_name = repository.get("full_name") if isinstance(repository, dict) else None
    repository_id = repository.get("id") if isinstance(repository, dict) else None
    ref = head.get("ref")
    sha = head.get("sha")
    if not isinstance(repository_id, int) or not all(
        isinstance(value, str) and value for value in (full_name, ref, sha)
    ):
        raise RuntimeError("pull request response has an incomplete head identity")
    return PullRequestHead(repository_id, full_name, ref, sha)


def select_stale_runs(
    runs: list[dict[str, Any]], current_head: PullRequestHead
) -> list[dict[str, Any]]:
    """Select prior-head runs for exactly the current fork repository and ref."""
    selected: dict[int, dict[str, Any]] = {}
    for run in runs:
        run_id = run.get("id")
        head_repository = run.get("head_repository")
        repository_id = head_repository.get("id") if isinstance(head_repository, dict) else None
        if not isinstance(run_id, int):
            continue
        if repository_id != current_head.repository_id:
            continue
        if run.get("head_branch") != current_head.ref:
            continue
        if run.get("head_sha") == current_head.sha:
            continue
        selected[run_id] = run
    return [selected[run_id] for run_id in sorted(selected)]


def cancel_stale_runs(
    api: ActionsApi, repository: str, pr_number: int, event_head_sha: str
) -> tuple[int, int]:
    pull_request = api.get_pull_request(repository, pr_number)
    current_head = _pull_request_head(pull_request)
    if pull_request.get("state") != "open" or current_head.sha != event_head_sha:
        print(
            "Ignoring a stale controller event: "
            f"event head {event_head_sha}, live head {current_head.sha}, "
            f"state {pull_request.get('state')}."
        )
        return 0, 0

    active_runs: list[dict[str, Any]] = []
    for status in ACTIVE_RUN_STATUSES:
        active_runs.extend(api.list_runs(repository, status))
    stale_runs = select_stale_runs(active_runs, current_head)
    if len(stale_runs) > MAX_STALE_RUNS:
        raise RuntimeError(
            f"refusing an unexpectedly large cancellation set ({len(stale_runs)} runs)"
        )

    cancelled = 0
    for run in stale_runs:
        run_id = int(run["id"])
        if api.cancel_run(repository, run_id):
            cancelled += 1
            print(
                f"Cancellation requested for {run.get('name', 'workflow')} "
                f"run {run_id} at {run.get('head_sha', '<unknown>')}."
            )
        else:
            print(f"Run {run_id} became terminal before cancellation; no action needed.")

    print(
        f"Found {len(stale_runs)} stale active run(s) for "
        f"{current_head.repository}:{current_head.ref}; requested {cancelled} cancellation(s)."
    )
    return len(stale_runs), cancelled


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--pr-number", required=True, type=int)
    parser.add_argument("--event-head-sha", required=True)
    args = parser.parse_args(argv)
    if not REPOSITORY_PATTERN.fullmatch(args.repository):
        parser.error("--repository must be an owner/repository pair")
    if args.pr_number <= 0:
        parser.error("--pr-number must be positive")
    if not SHA_PATTERN.fullmatch(args.event_head_sha):
        parser.error("--event-head-sha must be a 40-character hexadecimal commit SHA")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    api = GitHubApi(
        os.environ.get("GH_TOKEN", ""),
        os.environ.get("GITHUB_API_URL", "https://api.github.com"),
    )
    cancel_stale_runs(api, args.repository, args.pr_number, args.event_head_sha)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
