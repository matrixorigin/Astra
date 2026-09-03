#!/usr/bin/env python3
"""Cancel active GitHub Actions runs for a superseded pull-request head."""

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
from urllib.request import build_opener, HTTPRedirectHandler, Request


ACTIVE_RUN_STATUSES = frozenset(
    {"queued", "in_progress", "pending", "requested", "waiting"}
)
# These caps keep the worst-case request path within the workflow's five-minute
# timeout while leaving headroom over Astra's three pull-request workflows.
MAX_PAGES = 5
MAX_SUPERSEDED_RUNS = 10
REQUEST_TIMEOUT_SECONDS = 10
REPOSITORY_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
SHA_PATTERN = re.compile(r"^[0-9a-fA-F]{40}$")


@dataclass(frozen=True)
class SupersededHead:
    repository_id: int
    ref: str
    sha: str


class ActionsApi(Protocol):
    def list_runs(self, repository: str, head_sha: str) -> list[dict[str, Any]]: ...

    def cancel_run(self, repository: str, run_id: int) -> bool: ...


class RejectRedirects(HTTPRedirectHandler):
    def redirect_request(
        self,
        req: Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> Request | None:
        raise RuntimeError("refusing to forward the GitHub token through a redirect")


class GitHubApi:
    """Small GitHub REST client with bounded, origin-checked pagination."""

    def __init__(self, token: str, api_url: str = "https://api.github.com") -> None:
        if not token:
            raise ValueError("GH_TOKEN is required")
        self._token = token
        self._api_url = api_url.rstrip("/") + "/"
        parsed_api_url = urlsplit(self._api_url)
        if parsed_api_url.scheme.lower() != "https" or not parsed_api_url.netloc:
            raise ValueError("GITHUB_API_URL must use HTTPS and include a host")
        self._api_origin = (parsed_api_url.scheme.lower(), parsed_api_url.netloc.lower())
        self._opener = build_opener(RejectRedirects())

    def _request(self, method: str, url: str) -> tuple[bytes, Any]:
        target = url if urlsplit(url).scheme else urljoin(self._api_url, url.lstrip("/"))
        parsed_target = urlsplit(target)
        target_origin = (parsed_target.scheme.lower(), parsed_target.netloc.lower())
        if target_origin != self._api_origin:
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
        with self._opener.open(request, timeout=REQUEST_TIMEOUT_SECONDS) as response:
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

    def list_runs(self, repository: str, head_sha: str) -> list[dict[str, Any]]:
        query = urlencode(
            {"event": "pull_request", "head_sha": head_sha, "per_page": 100}
        )
        next_url: str | None = f"repos/{repository}/actions/runs?{query}"
        runs: list[dict[str, Any]] = []
        visited: set[str] = set()
        while next_url:
            if next_url in visited or len(visited) >= MAX_PAGES:
                raise RuntimeError("workflow-run pagination exceeded its safe bound")
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
            if error.code != 409:
                raise
            if error.fp is not None:
                error.close()
            run = self._get_json(f"repos/{repository}/actions/runs/{run_id}")
            if run.get("status") == "completed":
                return False
            raise RuntimeError(
                f"GitHub refused to cancel active workflow run {run_id}"
            ) from error
        return True


def select_superseded_runs(
    runs: list[dict[str, Any]], superseded_head: SupersededHead
) -> list[dict[str, Any]]:
    """Select active runs for exactly one superseded source-head generation."""
    selected: dict[int, dict[str, Any]] = {}
    for run in runs:
        run_id = run.get("id")
        head_repository = run.get("head_repository")
        repository_id = head_repository.get("id") if isinstance(head_repository, dict) else None
        if not isinstance(run_id, int) or run.get("status") not in ACTIVE_RUN_STATUSES:
            continue
        if repository_id != superseded_head.repository_id:
            continue
        if run.get("head_branch") != superseded_head.ref:
            continue
        if run.get("head_sha") != superseded_head.sha:
            continue
        selected[run_id] = run
    return [selected[run_id] for run_id in sorted(selected)]


def cancel_superseded_runs(
    api: ActionsApi, repository: str, superseded_head: SupersededHead
) -> tuple[int, int]:
    runs = api.list_runs(repository, superseded_head.sha)
    superseded_runs = select_superseded_runs(runs, superseded_head)
    if len(superseded_runs) > MAX_SUPERSEDED_RUNS:
        raise RuntimeError(
            "refusing an unexpectedly large cancellation set "
            f"({len(superseded_runs)} runs)"
        )

    cancelled = 0
    for run in superseded_runs:
        run_id = int(run["id"])
        if api.cancel_run(repository, run_id):
            cancelled += 1
            print(
                f"Cancellation requested for {run.get('name', 'workflow')} "
                f"run {run_id} at {superseded_head.sha}."
            )
        else:
            print(f"Run {run_id} became terminal before cancellation; no action needed.")

    print(
        f"Found {len(superseded_runs)} active run(s) for superseded head "
        f"repository={superseded_head.repository_id}, ref={superseded_head.ref}, "
        f"sha={superseded_head.sha}; requested {cancelled} cancellation(s)."
    )
    return len(superseded_runs), cancelled


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--head-repository-id", required=True, type=int)
    parser.add_argument("--head-ref", required=True)
    parser.add_argument("--superseded-head-sha", required=True)
    args = parser.parse_args(argv)
    if not REPOSITORY_PATTERN.fullmatch(args.repository):
        parser.error("--repository must be an owner/repository pair")
    if args.head_repository_id <= 0:
        parser.error("--head-repository-id must be positive")
    if not args.head_ref:
        parser.error("--head-ref must not be empty")
    if not SHA_PATTERN.fullmatch(args.superseded_head_sha):
        parser.error("--superseded-head-sha must be a 40-character hexadecimal SHA")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    api = GitHubApi(
        os.environ.get("GH_TOKEN", ""),
        os.environ.get("GITHUB_API_URL", "https://api.github.com"),
    )
    cancel_superseded_runs(
        api,
        args.repository,
        SupersededHead(
            repository_id=args.head_repository_id,
            ref=args.head_ref,
            sha=args.superseded_head_sha,
        ),
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
