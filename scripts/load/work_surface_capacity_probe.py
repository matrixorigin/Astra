#!/usr/bin/env python3
"""Measure bounded, owner-scoped Work execution reads under reader load.

The probe is deliberately read-only. It exercises the public Work projection
used by Web and TUI discovery, maps readers onto independent owners and
Sessions, and can make an explicit foreign-owner request for a 404 isolation
check. It reuses the repository's stdlib HTTP client so the load lane has no
extra package dependency.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import quote

from multi_cli_capacity_probe import (
    ProbeError,
    http_request,
    merge_base_url,
    percentile_summary,
    load_tokens,
    strip_bearer,
)


PROFILE_DEFAULTS = {
    "work-smoke": {
        "owners": 1,
        "sessions_per_owner": 1,
        "readers": 1,
        "active_views": 1,
        "rounds": 1,
    },
    "cross-surface-100": {
        "owners": 25,
        "sessions_per_owner": 4,
        "readers": 100,
        "active_views": 25,
        "rounds": 1,
    },
}
WORK_API_MAJOR_HEADER = "x-astra-work-api-major"
WORK_API_MAJOR = "1"


@dataclass(frozen=True)
class WorkRequest:
    request_id: int
    reader_index: int
    round_index: int
    owner_index: int
    session_index: int
    active_view_index: int | None


@dataclass
class WorkResult:
    request_id: int
    reader_index: int
    round_index: int
    owner_index: int
    session_index: int
    token_index: int | None
    http_status: int | None
    latency_ms: float
    body_bytes: int
    outcome: str
    error: str | None = None

    def to_json(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "reader_index": self.reader_index,
            "round_index": self.round_index,
            "owner_index": self.owner_index,
            "session_index": self.session_index,
            "token_index": self.token_index,
            "http_status": self.http_status,
            "latency_ms": round(self.latency_ms, 3),
            "body_bytes": self.body_bytes,
            "outcome": self.outcome,
            "error": self.error,
        }


class ResultWriter:
    def __init__(self, output_dir: Path) -> None:
        self.output_dir = output_dir
        self.output_dir.mkdir(parents=True, exist_ok=True)
        self.requests_path = output_dir / "requests.jsonl"
        self.summary_path = output_dir / "summary.json"
        self._file = self.requests_path.open("w", encoding="utf-8", buffering=1)
        self._lock = asyncio.Lock()

    async def write(self, result: WorkResult) -> None:
        async with self._lock:
            self._file.write(json.dumps(result.to_json(), sort_keys=True) + "\n")

    def summary(self, value: dict[str, Any]) -> None:
        self.summary_path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    def close(self) -> None:
        self._file.close()


def render_identifier(template: str, request: WorkRequest) -> str:
    try:
        value = template.format(
            request_id=request.request_id,
            reader_index=request.reader_index,
            round_index=request.round_index,
            owner_index=request.owner_index,
            session_index=request.session_index,
            active_view_index=(
                "" if request.active_view_index is None else request.active_view_index
            ),
        )
    except (KeyError, ValueError) as exc:
        raise ProbeError(f"invalid Work identifier template: {exc}") from exc
    value = value.strip()
    if not value or "/" in value or "?" in value or "#" in value:
        raise ProbeError("Work identifier templates must render one non-empty path segment")
    return value


def schedule_request(
    request_id: int,
    readers: int,
    owners: int,
    sessions_per_owner: int,
    active_views: int,
) -> WorkRequest:
    if readers <= 0 or owners <= 0 or sessions_per_owner <= 0:
        raise ProbeError("readers, owners, and sessions_per_owner must be positive")
    reader_index = request_id % readers
    round_index = request_id // readers
    owner_index = reader_index % owners
    session_index = (reader_index // owners) % sessions_per_owner
    active_view_index = owner_index if owner_index < active_views else None
    return WorkRequest(
        request_id=request_id,
        reader_index=reader_index,
        round_index=round_index,
        owner_index=owner_index,
        session_index=session_index,
        active_view_index=active_view_index,
    )


def validate_args(args: argparse.Namespace) -> None:
    defaults = PROFILE_DEFAULTS[args.profile]
    for name in ("owners", "sessions_per_owner", "readers", "active_views", "rounds", "concurrency"):
        value = getattr(args, name)
        if value is None:
            continue
        if value <= 0:
            raise ProbeError(f"--{name.replace('_', '-')} must be positive")
    if args.active_views > args.owners:
        raise ProbeError("--active-views cannot exceed --owners")
    if args.concurrency > args.readers:
        raise ProbeError("--concurrency cannot exceed --readers")
    if args.request_timeout_secs <= 0 or args.connect_timeout_secs <= 0:
        raise ProbeError("HTTP timeouts must be positive")
    if args.poll_interval_secs <= 0:
        raise ProbeError("--poll-interval-secs must be positive")
    if args.max_p95_ms is not None and args.max_p95_ms <= 0:
        raise ProbeError("--max-p95-ms must be positive")
    if args.max_p99_ms is not None and args.max_p99_ms <= 0:
        raise ProbeError("--max-p99-ms must be positive")
    if not args.dry_run:
        if not args.work_id_template or not args.branch_id_template:
            raise ProbeError("live probes require --work-id-template and --branch-id-template")
        if not args.tokens:
            raise ProbeError("no auth token; pass --auth-token or --token-file")
        if args.require_distinct_users and len(args.tokens) < args.owners:
            raise ProbeError(
                f"--require-distinct-users needs at least {args.owners} tokens; have {len(args.tokens)}"
            )
        if args.check_owner_isolation and not args.require_distinct_users:
            raise ProbeError("--check-owner-isolation requires --require-distinct-users")
    if args.check_owner_isolation and not args.work_id_template:
        raise ProbeError("--check-owner-isolation requires --work-id-template")
    if args.check_owner_isolation and args.owners < 2:
        raise ProbeError("--check-owner-isolation requires at least two owners")
    if args.check_owner_isolation and args.readers < args.owners * args.sessions_per_owner:
        raise ProbeError(
            "--check-owner-isolation needs at least one successful Work read per owner/Session pair"
        )
    if args.max_body_bytes <= 0:
        raise ProbeError("--max-body-bytes must be positive")
    del defaults


def percentile_values(results: list[WorkResult]) -> dict[str, float | None]:
    return percentile_summary([result.latency_ms for result in results])


def endpoint_url(base_url: str, work_id: str, branch_id: str) -> str:
    return merge_base_url(
        base_url,
        f"/v1/works/{quote(work_id, safe='')}/branches/{quote(branch_id, safe='')}/execution",
    )


def build_isolation_requests(
    args: argparse.Namespace,
    successful_targets: dict[tuple[int, int], tuple[str, str]],
    request_offset: int,
) -> tuple[list[tuple[WorkRequest, str, str, str]], list[str]]:
    """Pair each owner with an existing Work proven by the adjacent owner."""
    requests: list[tuple[WorkRequest, str, str, str]] = []
    violations: list[str] = []
    for source_owner in range(args.owners):
        source_token = args.tokens[source_owner % len(args.tokens)]
        for session_index in range(args.sessions_per_owner):
            source_target = successful_targets.get((source_owner, session_index))
            foreign_target = successful_targets.get(
                ((source_owner + 1) % args.owners, session_index)
            )
            if source_target is None or foreign_target is None:
                continue
            if source_target == foreign_target:
                violations.append(
                    "owner_isolation_target_identity_collision:"
                    f"owner={source_owner},session={session_index}"
                )
                continue
            isolation_request = WorkRequest(
                request_id=request_offset + len(requests),
                reader_index=source_owner,
                round_index=args.rounds,
                owner_index=source_owner,
                session_index=session_index,
                active_view_index=None,
            )
            requests.append(
                (isolation_request, source_token, foreign_target[0], foreign_target[1])
            )
    expected = args.owners * args.sessions_per_owner
    if not violations and len(requests) != expected:
        violations.append(f"owner_isolation_targets_missing:{len(requests)}/{expected}")
    return requests, violations


async def read_work_execution(
    args: argparse.Namespace,
    request: WorkRequest,
    token: str,
    work_id: str,
    branch_id: str,
    expected_status: int = 200,
) -> WorkResult:
    started = time.perf_counter()
    status: int | None = None
    body_bytes = 0
    try:
        response = await http_request(
            "GET",
            endpoint_url(args.base_url, work_id, branch_id),
            {
                "authorization": f"Bearer {strip_bearer(token)}",
                WORK_API_MAJOR_HEADER: WORK_API_MAJOR,
            },
            None,
            args.connect_timeout_secs,
            args.request_timeout_secs,
        )
        status = response.status
        body_bytes = len(response.body)
        if body_bytes > args.max_body_bytes:
            raise ProbeError(f"response exceeded {args.max_body_bytes} bytes")
        if status != expected_status:
            raise ProbeError(f"expected HTTP {expected_status}, received HTTP {status}")
        if expected_status == 200:
            try:
                payload = json.loads(response.body.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise ProbeError("execution projection was not valid JSON") from exc
            if not isinstance(payload, dict):
                raise ProbeError("execution projection was not a JSON object")
            if payload.get("work_id") != work_id or payload.get("branch_id") != branch_id:
                raise ProbeError("execution projection identity did not match the requested Work")
            if not isinstance(payload.get("generation"), int) or payload["generation"] < 0:
                raise ProbeError("execution projection has no valid generation")
        return WorkResult(
            request_id=request.request_id,
            reader_index=request.reader_index,
            round_index=request.round_index,
            owner_index=request.owner_index,
            session_index=request.session_index,
            token_index=None,
            http_status=status,
            latency_ms=(time.perf_counter() - started) * 1000.0,
            body_bytes=body_bytes,
            outcome="completed" if expected_status == 200 else "owner_isolation_pass",
        )
    except Exception as exc:  # noqa: BLE001
        return WorkResult(
            request_id=request.request_id,
            reader_index=request.reader_index,
            round_index=request.round_index,
            owner_index=request.owner_index,
            session_index=request.session_index,
            token_index=None,
            http_status=status,
            latency_ms=(time.perf_counter() - started) * 1000.0,
            body_bytes=body_bytes,
            outcome="failed" if expected_status == 200 else "owner_isolation_failed",
            error=str(exc),
        )


async def run_isolation_checks(
    args: argparse.Namespace,
    writer: ResultWriter,
    isolation_requests: list[tuple[WorkRequest, str, str, str]],
) -> list[WorkResult]:
    """Run foreign-owner checks with the same concurrency budget as reads."""
    semaphore = asyncio.Semaphore(args.concurrency)

    async def check_isolation(
        item: tuple[WorkRequest, str, str, str],
    ) -> WorkResult:
        isolation_request, token, foreign_work, foreign_branch = item
        async with semaphore:
            result = await read_work_execution(
                args,
                isolation_request,
                token,
                foreign_work,
                foreign_branch,
                expected_status=404,
            )
        result.token_index = isolation_request.owner_index % len(args.tokens)
        await writer.write(result)
        return result

    return list(await asyncio.gather(*(check_isolation(item) for item in isolation_requests)))


async def run_probe(args: argparse.Namespace) -> int:
    defaults = PROFILE_DEFAULTS[args.profile]
    args.owners = args.owners or defaults["owners"]
    args.sessions_per_owner = args.sessions_per_owner or defaults["sessions_per_owner"]
    args.readers = args.readers or defaults["readers"]
    args.active_views = args.active_views or defaults["active_views"]
    args.rounds = args.rounds or defaults["rounds"]
    args.concurrency = args.concurrency or min(args.readers, args.active_views)
    args.output_dir = args.output_dir or Path("tmp/capacity-probe") / f"work-surface-{args.profile}"
    args.tokens = load_tokens(args.token_file, args.auth_token or os.environ.get("ASTRA_AUTH_TOKEN"))
    validate_args(args)

    if args.dry_run:
        sample = schedule_request(0, args.readers, args.owners, args.sessions_per_owner, args.active_views)
        print(
            json.dumps(
                {
                    "profile": args.profile,
                    "base_url": args.base_url,
                    "owners": args.owners,
                    "sessions_per_owner": args.sessions_per_owner,
                    "readers": args.readers,
                    "active_views": args.active_views,
                    "rounds": args.rounds,
                    "concurrency": args.concurrency,
                    "total_requests": args.readers * args.rounds,
                    "estimated_active_poll_rps": round(args.active_views / args.poll_interval_secs, 3),
                    "work_id_example": render_identifier(args.work_id_template or "work-{owner_index}-{session_index}", sample),
                    "branch_id_example": render_identifier(args.branch_id_template or "branch-{owner_index}-{session_index}", sample),
                    "check_owner_isolation": args.check_owner_isolation,
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0

    writer = ResultWriter(args.output_dir)
    try:
        queue: asyncio.Queue[int] = asyncio.Queue()
        for request_id in range(args.readers * args.rounds):
            queue.put_nowait(request_id)
        results: list[WorkResult] = []
        results_lock = asyncio.Lock()
        targets_lock = asyncio.Lock()
        successful_targets: dict[tuple[int, int], tuple[str, str]] = {}

        async def worker(worker_id: int) -> None:
            while True:
                try:
                    request_id = queue.get_nowait()
                except asyncio.QueueEmpty:
                    return
                request = schedule_request(
                    request_id,
                    args.readers,
                    args.owners,
                    args.sessions_per_owner,
                    args.active_views,
                )
                token_index = request.owner_index % len(args.tokens)
                token = args.tokens[token_index]
                work_id = render_identifier(args.work_id_template, request)
                branch_id = render_identifier(args.branch_id_template, request)
                result = await read_work_execution(args, request, token, work_id, branch_id)
                result.token_index = token_index
                await writer.write(result)
                if result.outcome == "completed":
                    async with targets_lock:
                        successful_targets.setdefault((request.owner_index, request.session_index), (work_id, branch_id))
                async with results_lock:
                    results.append(result)
                    if len(results) % max(1, args.progress_every) == 0:
                        print(f"progress {len(results)}", file=sys.stderr)
                queue.task_done()

        started = time.perf_counter()
        await asyncio.gather(*(worker(index) for index in range(args.concurrency)))
        isolation_setup_violations: list[str] = []
        if args.check_owner_isolation:
            isolation_requests, isolation_setup_violations = build_isolation_requests(
                args,
                successful_targets,
                args.readers * args.rounds,
            )
            if not isolation_setup_violations:
                isolation_results = await run_isolation_checks(
                    args,
                    writer,
                    isolation_requests,
                )
                results.extend(isolation_results)
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        valid = [result for result in results if result.outcome == "completed"]
        isolation = [result for result in results if result.outcome == "owner_isolation_pass"]
        failures = [result for result in results if result.outcome.endswith("failed")]
        latency = percentile_values(valid)
        violations: list[str] = []
        violations.extend(isolation_setup_violations)
        if len(valid) != args.readers * args.rounds:
            violations.append(f"successful_reads:{len(valid)}/{args.readers * args.rounds}")
        if args.check_owner_isolation:
            expected_isolation_checks = args.owners * args.sessions_per_owner
            if len(isolation) != expected_isolation_checks:
                violations.append(f"owner_isolation:{len(isolation)}/{expected_isolation_checks}")
        if args.max_p95_ms is not None and latency["p95"] is not None and latency["p95"] > args.max_p95_ms:
            violations.append(f"p95_ms:{latency['p95']:.3f}>{args.max_p95_ms:g}")
        if args.max_p99_ms is not None and latency["p99"] is not None and latency["p99"] > args.max_p99_ms:
            violations.append(f"p99_ms:{latency['p99']:.3f}>{args.max_p99_ms:g}")
        summary = {
            "profile": args.profile,
            "base_url": args.base_url,
            "owners": args.owners,
            "sessions_per_owner": args.sessions_per_owner,
            "readers": args.readers,
            "active_views": args.active_views,
            "rounds": args.rounds,
            "concurrency": args.concurrency,
            "total_requests": args.readers * args.rounds,
            "elapsed_ms": round(elapsed_ms, 3),
            "throughput_rps": round(len(results) / (elapsed_ms / 1000.0), 3) if elapsed_ms else 0,
            "completed": len(valid),
            "owner_isolation_passed": len(isolation),
            "failed": len(failures),
            "latency_ms": latency,
            "estimated_active_poll_rps": round(args.active_views / args.poll_interval_secs, 3),
            "contract_violations": violations,
            "failure_examples": [result.to_json() for result in failures[:10]],
        }
        writer.summary(summary)
        print(json.dumps(summary, indent=2, sort_keys=True))
        return 0 if not violations and not failures else 2
    finally:
        writer.close()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILE_DEFAULTS), default="work-smoke")
    parser.add_argument("--base-url", default="http://127.0.0.1:3000")
    parser.add_argument("--work-id-template")
    parser.add_argument("--branch-id-template")
    parser.add_argument("--auth-token")
    parser.add_argument("--token-file")
    parser.add_argument("--owners", type=int)
    parser.add_argument("--sessions-per-owner", type=int)
    parser.add_argument("--readers", type=int)
    parser.add_argument("--active-views", type=int)
    parser.add_argument("--rounds", type=int)
    parser.add_argument("--concurrency", type=int)
    parser.add_argument("--poll-interval-secs", type=float, default=1.2)
    parser.add_argument("--connect-timeout-secs", type=float, default=5.0)
    parser.add_argument("--request-timeout-secs", type=float, default=30.0)
    parser.add_argument("--max-body-bytes", type=int, default=64 * 1024)
    parser.add_argument("--max-p95-ms", type=float)
    parser.add_argument("--max-p99-ms", type=float)
    parser.add_argument("--require-distinct-users", action="store_true")
    parser.add_argument("--check-owner-isolation", action="store_true")
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--progress-every", type=int, default=25)
    parser.add_argument("--dry-run", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return asyncio.run(run_probe(args))
    except ProbeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
