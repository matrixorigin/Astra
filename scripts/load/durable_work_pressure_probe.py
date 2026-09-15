#!/usr/bin/env python3
"""Exercise the public durable Work protocol with mixed readers and writers.

The probe creates a bounded owner/session-shaped dataset, opens two read
attachments per Work, and then runs catalog, Work, event, transcript and Task
Graph reads while attention-cursor writes continue.  It uses only the Python
standard library and the same HTTP/metrics primitives as
``multi_cli_capacity_probe.py``.  The probe deliberately does not invoke an
LLM: its result is a database and public-protocol signal, not an agent quality
or UI end-to-end result.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import secrets
import sys
import time
from collections import Counter
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from multi_cli_capacity_probe import (
    ProbeError,
    HttpResponse,
    http_request,
    merge_base_url,
    parse_prometheus_metrics,
    percentile_summary,
    strip_bearer,
)


PROFILE_DEFAULTS: dict[str, dict[str, float | int]] = {
    "smoke": {
        "owners": 3,
        "sessions_per_owner": 2,
        "duration_secs": 10.0,
        "read_rate": 2.0,
        "write_rate": 1.0,
        "metrics_interval_secs": 2.0,
    },
    "pressure": {
        "owners": 25,
        "sessions_per_owner": 4,
        "duration_secs": 60.0,
        "read_rate": 4.0,
        "write_rate": 1.0,
        "metrics_interval_secs": 5.0,
    },
}

WORK_API_MAJOR = "1"
MAX_RESPONSE_BYTES = 512 * 1024
READ_OPERATIONS = ("catalog", "work", "branches", "events", "task_graph", "transcript")


@dataclass(frozen=True)
class ProbeConfig:
    base_url: str
    owners: int
    sessions_per_owner: int
    duration_secs: float
    read_rate: float
    write_rate: float
    metrics_interval_secs: float
    connect_timeout_secs: float
    request_timeout_secs: float
    register_users: bool
    register_prefix: str
    register_password: str
    register_domain: str
    register_concurrency: int
    auth_tokens: tuple[str, ...]
    output_dir: Path
    slow_session_index: int
    slow_delay_secs: float
    require_metrics: bool

    @property
    def total_sessions(self) -> int:
        return self.owners * self.sessions_per_owner


@dataclass(frozen=True)
class WorkTarget:
    owner_index: int
    session_index: int
    token: str
    work_id: str
    branch_id: str
    attachment_ids: tuple[str, ...]
    attachment_epochs: tuple[int, ...]

    @property
    def key(self) -> str:
        return f"owner-{self.owner_index}/session-{self.session_index}"


@dataclass
class OperationResult:
    operation: str
    owner_index: int
    session_index: int | None
    status: int | None
    latency_ms: float
    response_bytes: int
    outcome: str
    error_code: str | None = None
    error: str | None = None

    def to_json(self) -> dict[str, Any]:
        return {
            "operation": self.operation,
            "owner_index": self.owner_index,
            "session_index": self.session_index,
            "status": self.status,
            "latency_ms": round(self.latency_ms, 3),
            "response_bytes": self.response_bytes,
            "outcome": self.outcome,
            "error_code": self.error_code,
            "error": self.error,
        }


@dataclass
class MetricsSampler:
    config: ProbeConfig
    samples: list[dict[str, Any]] = field(default_factory=list)
    stop: asyncio.Event = field(default_factory=asyncio.Event)

    async def sample_once(self, kind: str) -> None:
        started = time.perf_counter()
        headers = {}
        # Metrics are operator-facing.  A separate token is intentionally not
        # inferred from a Work owner token.
        metrics_token = os.environ.get("ASTRA_METRICS_AUTH_TOKEN")
        if metrics_token:
            headers["authorization"] = f"Bearer {strip_bearer(metrics_token)}"
        try:
            response = await http_request(
                "GET",
                merge_base_url(self.config.base_url, "/metrics"),
                headers,
                None,
                self.config.connect_timeout_secs,
                min(self.config.request_timeout_secs, 30.0),
            )
            raw = response.body.decode("utf-8", errors="replace")
            self.samples.append(
                {
                    "kind": kind,
                    "unix_ms": int(time.time() * 1000),
                    "http_status": response.status,
                    "latency_ms": round((time.perf_counter() - started) * 1000.0, 3),
                    "metrics": parse_prometheus_metrics(raw),
                }
            )
        except Exception as exc:  # noqa: BLE001
            self.samples.append(
                {
                    "kind": kind,
                    "unix_ms": int(time.time() * 1000),
                    "http_status": None,
                    "latency_ms": round((time.perf_counter() - started) * 1000.0, 3),
                    "metrics": {},
                    "error": str(exc),
                }
            )

    async def run(self) -> None:
        while not self.stop.is_set():
            await self.sample_once("periodic")
            try:
                await asyncio.wait_for(
                    self.stop.wait(), timeout=self.config.metrics_interval_secs
                )
            except asyncio.TimeoutError:
                pass


def positive_int(value: int | None, default: int, name: str) -> int:
    resolved = default if value is None else value
    if resolved <= 0:
        raise ProbeError(f"--{name} must be positive")
    return resolved


def positive_float(value: float | None, default: float, name: str) -> float:
    resolved = default if value is None else value
    if not math.isfinite(resolved) or resolved <= 0:
        raise ProbeError(f"--{name} must be positive")
    return resolved


def parse_tokens(path: str | None, explicit: str | None) -> list[str]:
    values: list[str] = []
    if explicit:
        values.append(strip_bearer(explicit))
    if path:
        raw = Path(path).read_text(encoding="utf-8").strip()
        if raw:
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                parsed = None
            if isinstance(parsed, list):
                for item in parsed:
                    if isinstance(item, str):
                        values.append(strip_bearer(item))
                    elif isinstance(item, dict) and isinstance(item.get("access_token"), str):
                        values.append(strip_bearer(item["access_token"]))
            elif isinstance(parsed, dict):
                token = parsed.get("access_token")
                if isinstance(token, str):
                    values.append(strip_bearer(token))
                tokens = parsed.get("tokens", parsed.get("access_tokens", []))
                if isinstance(tokens, list):
                    values.extend(strip_bearer(item) for item in tokens if isinstance(item, str))
            else:
                values.extend(
                    strip_bearer(line)
                    for line in raw.splitlines()
                    if line.strip() and not line.lstrip().startswith("#")
                )
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        if value and value not in seen:
            seen.add(value)
            result.append(value)
    return result


async def register_or_login(config: ProbeConfig, index: int) -> str:
    username = f"{config.register_prefix}-{index:05d}"
    body = json.dumps(
        {
            "username": username,
            "email": f"{username}@{config.register_domain}",
            "password": config.register_password,
            "display_name": username,
        },
        separators=(",", ":"),
    ).encode("utf-8")
    headers = {"content-type": "application/json"}
    response = await http_request(
        "POST",
        merge_base_url(config.base_url, "/auth/register"),
        headers,
        body,
        config.connect_timeout_secs,
        config.request_timeout_secs,
    )
    if response.status not in range(200, 300):
        login_body = json.dumps(
            {"username": username, "password": config.register_password},
            separators=(",", ":"),
        ).encode("utf-8")
        response = await http_request(
            "POST",
            merge_base_url(config.base_url, "/auth/login"),
            headers,
            login_body,
            config.connect_timeout_secs,
            config.request_timeout_secs,
        )
    if response.status not in range(200, 300):
        raise ProbeError(f"auth bootstrap failed for owner {index}: HTTP {response.status}")
    try:
        parsed = json.loads(response.body.decode("utf-8"))
    except json.JSONDecodeError as exc:
        raise ProbeError(f"auth bootstrap returned non-JSON for owner {index}") from exc
    token = parsed.get("access_token") if isinstance(parsed, dict) else None
    if not isinstance(token, str) or not token:
        raise ProbeError(f"auth bootstrap response missing access_token for owner {index}")
    return strip_bearer(token)


async def resolve_tokens(config: ProbeConfig) -> list[str]:
    if config.register_users:
        semaphore = asyncio.Semaphore(config.register_concurrency)
        values: list[str | None] = [None] * config.owners

        async def worker(index: int) -> None:
            async with semaphore:
                values[index] = await register_or_login(config, index)

        await asyncio.gather(*(worker(index) for index in range(config.owners)))
        return [value for value in values if value]
    if not config.auth_tokens:
        raise ProbeError(
            "no auth token available; pass --auth-token/--token-file or --register-users"
        )
    if len(set(config.auth_tokens)) < config.owners:
        raise ProbeError("multi-user pressure requires one distinct auth token per owner")
    if len(config.auth_tokens) < config.owners:
        raise ProbeError(
            f"need one token per owner ({config.owners}), received {len(config.auth_tokens)}"
        )
    return list(config.auth_tokens[: config.owners])


def request_error_code(response: HttpResponse) -> str | None:
    try:
        value = json.loads(response.body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None
    return error_code_from_value(value)


def error_code_from_value(value: Any) -> str | None:
    if not isinstance(value, dict):
        return None
    nested = value.get("error") if isinstance(value.get("error"), dict) else {}
    for source in (value, nested):
        for key in ("error_code", "code"):
            candidate = source.get(key)
            if isinstance(candidate, str) and candidate:
                return candidate
    return None


async def request_json(
    config: ProbeConfig,
    operation: str,
    owner_index: int,
    session_index: int | None,
    token: str,
    method: str,
    path: str,
    payload: dict[str, Any] | None = None,
    allow_empty: bool = False,
) -> tuple[OperationResult, Any | None]:
    body = (
        json.dumps(payload, separators=(",", ":")).encode("utf-8")
        if payload is not None
        else None
    )
    headers = {
        "authorization": f"Bearer {token}",
        "x-astra-work-api-major": WORK_API_MAJOR,
    }
    if body is not None:
        headers["content-type"] = "application/json"
    started = time.perf_counter()
    response: HttpResponse | None = None
    try:
        response = await http_request(
            method,
            merge_base_url(config.base_url, path),
            headers,
            body,
            config.connect_timeout_secs,
            config.request_timeout_secs,
        )
        latency_ms = (time.perf_counter() - started) * 1000.0
        if response.status not in range(200, 300):
            error_value: Any | None = None
            if len(response.body) <= MAX_RESPONSE_BYTES:
                try:
                    error_value = json.loads(response.body.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError):
                    pass
            return (
                OperationResult(
                    operation,
                    owner_index,
                    session_index,
                    response.status,
                    latency_ms,
                    len(response.body),
                    "http_error",
                    request_error_code(response),
                ),
                error_value,
            )
        if len(response.body) > MAX_RESPONSE_BYTES:
            return (
                OperationResult(
                    operation,
                    owner_index,
                    session_index,
                    response.status,
                    latency_ms,
                    len(response.body),
                    "response_too_large",
                    error="response exceeds bounded probe budget",
                ),
                None,
            )
        try:
            value = json.loads(response.body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            if allow_empty and not response.body:
                return (
                    OperationResult(
                        operation,
                        owner_index,
                        session_index,
                        response.status,
                        latency_ms,
                        0,
                        "ok",
                    ),
                    None,
                )
            return (
                OperationResult(
                    operation,
                    owner_index,
                    session_index,
                    response.status,
                    latency_ms,
                    len(response.body),
                    "invalid_json",
                    error=str(exc),
                ),
                None,
            )
        return (
            OperationResult(
                operation,
                owner_index,
                session_index,
                response.status,
                latency_ms,
                len(response.body),
                "ok",
            ),
            value,
        )
    except Exception as exc:  # noqa: BLE001
        return (
            OperationResult(
                operation,
                owner_index,
                session_index,
                response.status if response else None,
                (time.perf_counter() - started) * 1000.0,
                len(response.body) if response else 0,
                "transport_error",
                error=str(exc),
            ),
            None,
        )


async def create_target(
    config: ProbeConfig, owner_index: int, session_index: int, token: str
) -> tuple[WorkTarget, list[OperationResult]]:
    request_id = f"durable-pressure-{owner_index}-{session_index}-{secrets.token_hex(8)}"
    payload = {
        "request_id": request_id,
        "goal": f"Durable pressure owner {owner_index} session {session_index}",
        "criteria": [],
    }
    result, value = await request_json(
        config,
        "create",
        owner_index,
        session_index,
        token,
        "POST",
        "/v1/works",
        payload,
    )
    if result.outcome != "ok" or not isinstance(value, dict):
        raise ProbeError(f"create Work failed for {owner_index}/{session_index}: {result}")
    overview = value.get("overview")
    branch = overview.get("delivery_branch") if isinstance(overview, dict) else None
    work_id = overview.get("work_id") if isinstance(overview, dict) else None
    branch_id = branch.get("branch_id") if isinstance(branch, dict) else None
    if not all(isinstance(item, str) and item for item in (work_id, branch_id)):
        raise ProbeError(f"create Work response missing canonical identity: {value}")
    results = [result]

    # Lost-response recovery is modeled with the same request identity.  A
    # retry must return the exact Work projection and must not create a second
    # Work.  The first response is retained in memory, so no credential or raw
    # request body is written to the probe report.
    retry_result, retry_value = await request_json(
        config,
        "create_retry",
        owner_index,
        session_index,
        token,
        "POST",
        "/v1/works",
        payload,
    )
    results.append(retry_result)
    if retry_result.outcome != "ok" or retry_value != value:
        raise ProbeError(
            f"idempotent Work retry changed the projection for {owner_index}/{session_index}"
        )

    attachment_ids: list[str] = []
    attachment_epochs: list[int] = []
    for attachment_index in range(2):
        attach_result, attachment = await request_json(
            config,
            "attach",
            owner_index,
            session_index,
            token,
            "POST",
            f"/v1/works/{work_id}/branches/{branch_id}/attachments",
            {"request_id": f"{request_id}-attachment-{attachment_index}"},
        )
        results.append(attach_result)
        attachment_id = attachment.get("attachment_id") if isinstance(attachment, dict) else None
        attachment_epoch = attachment.get("attachment_epoch") if isinstance(attachment, dict) else None
        if (
            attach_result.outcome != "ok"
            or not isinstance(attachment, dict)
            or attachment.get("work_id") != work_id
            or attachment.get("branch_id") != branch_id
            or attachment.get("mode") != "read_only"
            or "session_id" in attachment
            or not isinstance(attachment_id, str)
            or not attachment_id
            or not isinstance(attachment_epoch, int)
            or attachment_epoch < 1
        ):
            raise ProbeError(
                f"read attachment violated its typed contract for {owner_index}/{session_index}: {attach_result}"
            )
        attachment_ids.append(attachment_id)
        attachment_epochs.append(attachment_epoch)
    if len(set(attachment_ids)) != 2:
        raise ProbeError("a Work must expose two distinct read attachment identities")
    return (
        WorkTarget(
            owner_index,
            session_index,
            token,
            work_id,
            branch_id,
            tuple(attachment_ids),
            tuple(attachment_epochs),
        ),
        results,
    )


async def warm_target(config: ProbeConfig, target: WorkTarget) -> list[OperationResult]:
    operations = [
        ("work", "GET", f"/v1/works/{target.work_id}", None),
        ("branches", "GET", f"/v1/works/{target.work_id}/branches", None),
        ("events", "GET", f"/v1/works/{target.work_id}/events?limit=16", None),
        (
            "task_graph",
            "GET",
            f"/v1/works/{target.work_id}/branches/{target.branch_id}/task-graph?item_limit=16&dependency_limit=16",
            None,
        ),
        (
            "transcript",
            "GET",
            f"/v1/works/{target.work_id}/branches/{target.branch_id}/transcript?limit=16",
            None,
        ),
    ]
    results: list[OperationResult] = []
    for operation, _method, _path, _payload in operations:
        result = await run_read(config, target, operation)
        results.append(result)
        if result.outcome != "ok":
            raise ProbeError(f"warm-up {operation} failed for {target.key}: {result}")
    # The genesis Work always has event 1.  A same cursor PUT is a durable,
    # owner-scoped write and is intentionally safe to repeat under pressure.
    cursor_result, cursor = await request_json(
        config,
        "read_cursor",
        target.owner_index,
        target.session_index,
        target.token,
        "PUT",
        f"/v1/works/{target.work_id}/read-cursor",
        {"through_event_seq": 1},
    )
    results.append(cursor_result)
    if cursor_result.outcome != "ok" or not isinstance(cursor, dict):
        raise ProbeError(f"read cursor write failed for {target.key}: {cursor_result}")
    return results


def read_path(target: WorkTarget, operation: str) -> str:
    if operation == "catalog":
        return "/v1/works?limit=16"
    if operation == "work":
        return f"/v1/works/{target.work_id}"
    if operation == "branches":
        return f"/v1/works/{target.work_id}/branches"
    if operation == "events":
        return f"/v1/works/{target.work_id}/events?limit=16"
    if operation == "task_graph":
        return (
            f"/v1/works/{target.work_id}/branches/{target.branch_id}/task-graph"
            "?item_limit=16&dependency_limit=16"
        )
    if operation == "transcript":
        return f"/v1/works/{target.work_id}/branches/{target.branch_id}/transcript?limit=16"
    raise ValueError(f"unknown read operation {operation}")


async def run_read(config: ProbeConfig, target: WorkTarget, operation: str) -> OperationResult:
    result, value = await request_json(
        config,
        operation,
        target.owner_index,
        target.session_index,
        target.token,
        "GET",
        read_path(target, operation),
    )
    if result.outcome == "ok":
        identity_ok = True
        if operation == "work":
            overview = value.get("overview") if isinstance(value, dict) else None
            identity_ok = (
                isinstance(overview, dict)
                and overview.get("work_id") == target.work_id
                and isinstance(overview.get("delivery_branch"), dict)
                and overview["delivery_branch"].get("branch_id") == target.branch_id
            )
        elif operation == "branches":
            identity_ok = (
                isinstance(value, dict)
                and value.get("work_id") == target.work_id
                and value.get("delivery_branch_id") == target.branch_id
            )
        elif operation == "events":
            identity_ok = isinstance(value, dict) and value.get("work_id") == target.work_id
        elif operation == "task_graph":
            basis = value.get("basis") if isinstance(value, dict) else None
            identity_ok = (
                isinstance(basis, dict)
                and basis.get("work_id") == target.work_id
                and basis.get("branch_id") == target.branch_id
            )
        elif operation == "transcript":
            identity_ok = (
                isinstance(value, dict)
                and value.get("work_id") == target.work_id
                and value.get("branch_id") == target.branch_id
            )
        if not identity_ok:
            result.outcome = "identity_mismatch"
            result.error = "successful projection failed its typed Work/branch identity check"
    return result


async def run_write(
    config: ProbeConfig, target: WorkTarget, write_index: int, last_epoch: int | None
) -> tuple[list[OperationResult], int | None]:
    attach_result, attachment = await request_json(
        config,
        "write_attachment_open",
        target.owner_index,
        target.session_index,
        target.token,
        "POST",
        f"/v1/works/{target.work_id}/branches/{target.branch_id}/attachments",
        {"request_id": f"durable-pressure-write-{target.owner_index}-{write_index}-{secrets.token_hex(6)}"},
    )
    results = [attach_result]
    attachment_id = attachment.get("attachment_id") if isinstance(attachment, dict) else None
    attachment_epoch = attachment.get("attachment_epoch") if isinstance(attachment, dict) else None
    if (
        attach_result.outcome != "ok"
        or not isinstance(attachment, dict)
        or attachment.get("work_id") != target.work_id
        or attachment.get("branch_id") != target.branch_id
        or attachment.get("mode") != "read_only"
        or "session_id" in attachment
        or not isinstance(attachment_id, str)
        or attachment_id in target.attachment_ids
        or not isinstance(attachment_epoch, int)
        or attachment_epoch < 1
    ):
        attach_result.outcome = "identity_mismatch"
        attach_result.error = "write attachment failed its typed identity contract"
        return results, last_epoch
    if last_epoch is not None and attachment_epoch <= last_epoch:
        attach_result.outcome = "epoch_not_monotonic"
        attach_result.error = "attachment epoch did not advance after a durable write"
    detach_result, _ = await request_json(
        config,
        "write_attachment_close",
        target.owner_index,
        target.session_index,
        target.token,
        "DELETE",
        f"/v1/works/{target.work_id}/branches/{target.branch_id}/attachments/{attachment_id}",
        allow_empty=True,
    )
    results.append(detach_result)
    cursor_result, _ = await request_json(
        config,
        "read_cursor",
        target.owner_index,
        target.session_index,
        target.token,
        "PUT",
        f"/v1/works/{target.work_id}/read-cursor",
        {"through_event_seq": 1},
    )
    results.append(cursor_result)
    return results, attachment_epoch


async def mixed_worker(
    config: ProbeConfig,
    target: WorkTarget,
    results: list[OperationResult],
    results_lock: asyncio.Lock,
    deadline: float,
    writer: bool,
) -> None:
    interval = 1.0 / (config.write_rate if writer else config.read_rate)
    operation_index = 0
    write_index = 0
    last_epoch: int | None = max(target.attachment_epochs)
    while time.monotonic() < deadline:
        if (
            target.session_index + target.owner_index * config.sessions_per_owner
            == config.slow_session_index
        ):
            await asyncio.sleep(config.slow_delay_secs)
        if writer:
            write_task = asyncio.create_task(
                run_write(config, target, write_index, last_epoch)
            )
            read_task = asyncio.create_task(
                run_read(config, target, READ_OPERATIONS[operation_index % len(READ_OPERATIONS)])
            )
            batch, last_epoch = await write_task
            write_index += 1
            # Keep one reader on the same Work as every active writer.  This
            # is the observation that catches a torn attachment projection;
            # the other session-shaped targets remain read-only readers.
            batch.append(await read_task)
            operation_index += 1
        else:
            operation = READ_OPERATIONS[operation_index % len(READ_OPERATIONS)]
            operation_index += 1
            batch = [await run_read(config, target, operation)]
        async with results_lock:
            results.extend(batch)
        await asyncio.sleep(interval)


async def check_foreign_access(config: ProbeConfig, source: WorkTarget, foreign: WorkTarget) -> list[OperationResult]:
    results: list[OperationResult] = []
    for operation, path, expected_code in (
        ("foreign_work", f"/v1/works/{source.work_id}", "work_not_found"),
        ("foreign_events", f"/v1/works/{source.work_id}/events?limit=1", "work_not_found"),
        (
            "foreign_task_graph",
            f"/v1/works/{source.work_id}/branches/{source.branch_id}/task-graph?item_limit=1&dependency_limit=1",
            "branch_not_found",
        ),
    ):
        result, value = await request_json(
            config,
            operation,
            foreign.owner_index,
            foreign.session_index,
            foreign.token,
            "GET",
            path,
        )
        error_code = error_code_from_value(value)
        if result.status != 404:
            result.outcome = "foreign_access_allowed"
            result.error = "foreign owner observed a non-404 response"
        elif value is None:
            result.outcome = "foreign_rejection_unparseable"
            result.error = "foreign rejection did not provide a bounded JSON error"
        elif source.work_id in json.dumps(value):
            result.outcome = "foreign_identity_leak"
            result.error = "foreign rejection echoed the Work identity"
        elif error_code != expected_code:
            result.outcome = "foreign_error_code_mismatch"
            result.error = f"expected typed error code {expected_code!r}, got {error_code!r}"
        else:
            result.outcome = "expected_rejection"
        results.append(result)
    return results


def summarize_operation_results(results: list[OperationResult]) -> dict[str, Any]:
    grouped: dict[str, list[OperationResult]] = {}
    for result in results:
        grouped.setdefault(result.operation, []).append(result)
    summary: dict[str, Any] = {}
    for operation, values in sorted(grouped.items()):
        summary[operation] = {
            "samples": len(values),
            "outcomes": dict(sorted(Counter(value.outcome for value in values).items())),
            "status": dict(sorted(Counter(str(value.status or "none") for value in values).items())),
            "latency_ms": percentile_summary([value.latency_ms for value in values]),
            "response_bytes": percentile_summary([float(value.response_bytes) for value in values]),
            "errors": [
                {"status": value.status, "error_code": value.error_code, "error": value.error}
                for value in values
                if value.outcome not in ("ok", "expected_rejection")
            ][:20],
        }
    return summary


def summarize_metrics(samples: list[dict[str, Any]]) -> dict[str, Any]:
    metric_samples = [sample for sample in samples if sample.get("metrics")]
    successful_metric_samples = [
        sample
        for sample in metric_samples
        if isinstance(sample.get("http_status"), int)
        and 200 <= sample["http_status"] < 300
    ]
    first = successful_metric_samples[0].get("metrics", {}) if successful_metric_samples else {}
    last = successful_metric_samples[-1].get("metrics", {}) if successful_metric_samples else {}
    deltas: dict[str, float] = {}
    for key, value in last.items():
        if not isinstance(value, (int, float)):
            continue
        before = first.get(key, 0)
        if isinstance(before, (int, float)):
            deltas[key] = max(0.0, float(value) - float(before))
    return {
        "sample_count": len(samples),
        "samples_with_metrics": len(metric_samples),
        "successful_samples_with_metrics": len(successful_metric_samples),
        "http_status": dict(sorted(Counter(str(sample.get("http_status", "none")) for sample in samples).items())),
        "errors": sum(1 for sample in samples if sample.get("error")),
        "counter_deltas": dict(sorted(deltas.items())),
        "missing_expected_families": [
            prefix
            for prefix in ("astra_run_admission_", "astra_durable_run_event_", "astra_event_ingestion_")
            if not any(key.startswith(prefix) for key in last)
        ],
    }


async def run_probe(config: ProbeConfig) -> dict[str, Any]:
    tokens = await resolve_tokens(config)
    targets: list[WorkTarget] = []
    lifecycle_results: list[OperationResult] = []
    for owner_index in range(config.owners):
        for session_index in range(config.sessions_per_owner):
            target, results = await create_target(
                config, owner_index, session_index, tokens[owner_index]
            )
            targets.append(target)
            lifecycle_results.extend(results)
    for target in targets:
        lifecycle_results.extend(await warm_target(config, target))

    if len(targets) != config.total_sessions or any(len(target.attachment_ids) != 2 for target in targets):
        raise ProbeError("dataset did not produce two independent read attachments per Work")

    # The explicit rejection check is performed before the pressure window so
    # an accidental ownership leak fails fast and never gets hidden by a later
    # successful read count.
    lifecycle_results.extend(await check_foreign_access(config, targets[0], targets[-1]))

    sampler = MetricsSampler(config)
    sampler_task = asyncio.create_task(sampler.run())
    pressure_results: list[OperationResult] = []
    results_lock = asyncio.Lock()
    deadline = time.monotonic() + config.duration_secs
    tasks: list[asyncio.Task[None]] = []
    for target in targets:
        tasks.append(
            asyncio.create_task(
                mixed_worker(
                    config,
                    target,
                    pressure_results,
                    results_lock,
                    deadline,
                    writer=target.session_index == 0,
                )
            )
        )
    await asyncio.gather(*tasks)
    sampler.stop.set()
    await sampler_task
    await sampler.sample_once("final")

    all_results = lifecycle_results + pressure_results
    pressure_failures = [
        result
        for result in pressure_results
        if result.outcome != "ok"
    ]
    foreign_results = [
        result
        for result in lifecycle_results
        if result.operation.startswith("foreign_")
    ]
    pressure_by_target_operation = Counter(
        (result.owner_index, result.session_index, result.operation)
        for result in pressure_results
    )
    missing_read_coverage = [
        {
            "owner_index": target.owner_index,
            "session_index": target.session_index,
            "operation": operation,
        }
        for target in targets
        for operation in READ_OPERATIONS
        if pressure_by_target_operation[
            (target.owner_index, target.session_index, operation)
        ]
        < 1
    ]
    missing_write_coverage = [
        {
            "owner_index": target.owner_index,
            "session_index": target.session_index,
            "operation": operation,
        }
        for target in targets
        if target.session_index == 0
        for operation in ("write_attachment_open", "write_attachment_close", "read_cursor")
        if pressure_by_target_operation[
            (target.owner_index, target.session_index, operation)
        ]
        < 1
    ]
    pressure_coverage_complete = not missing_read_coverage and not missing_write_coverage
    identities_stable = True
    for target in targets:
        result, value = await request_json(
            config,
            "identity_check",
            target.owner_index,
            target.session_index,
            target.token,
            "GET",
            f"/v1/works/{target.work_id}",
        )
        all_results.append(result)
        if result.outcome != "ok" or not isinstance(value, dict):
            identities_stable = False
            continue
        overview = value.get("overview") if isinstance(value, dict) else None
        returned_id = overview.get("work_id") if isinstance(overview, dict) else None
        returned_branch = (
            overview.get("delivery_branch", {}).get("branch_id")
            if isinstance(overview, dict) and isinstance(overview.get("delivery_branch"), dict)
            else None
        )
        identities_stable = identities_stable and returned_id == target.work_id and returned_branch == target.branch_id

    target_results = {
        (target.owner_index, target.session_index): [
            result
            for result in pressure_results
            if result.owner_index == target.owner_index
            and result.session_index == target.session_index
        ]
        for target in targets
    }
    other_target_keys = {
        (target.owner_index, target.session_index)
        for target in targets
        if target.owner_index * config.sessions_per_owner + target.session_index
        != config.slow_session_index
    }
    other_sessions_available = all(
        target_results[key] and all(result.outcome == "ok" for result in target_results[key])
        for key in other_target_keys
    )
    invariants = {
        "work_branch_identity_stable": identities_stable,
        "two_read_attachments_per_work": all(len(target.attachment_ids) == 2 for target in targets),
        "idempotent_work_create_retry": all(
            result.outcome == "ok" for result in lifecycle_results if result.operation == "create_retry"
        ),
        "foreign_owner_rejected_without_identity_leak": bool(foreign_results)
        and all(result.outcome == "expected_rejection" for result in foreign_results),
        "pressure_coverage_complete": pressure_coverage_complete,
        "mixed_pressure_requests_succeeded": not pressure_failures,
        "other_sessions_remained_available": other_sessions_available,
    }
    metrics_summary = summarize_metrics(sampler.samples)
    if config.require_metrics and metrics_summary["successful_samples_with_metrics"] == 0:
        invariants["metrics_available"] = False
    else:
        invariants["metrics_available"] = True
    ok = all(invariants.values())
    return {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "profile": {
            "owners": config.owners,
            "sessions_per_owner": config.sessions_per_owner,
            "duration_secs": config.duration_secs,
            "read_rate_per_session": config.read_rate,
            "write_rate_per_active_session": config.write_rate,
            "slow_session_index": config.slow_session_index,
            "slow_delay_secs": config.slow_delay_secs,
        },
        "dataset": {
            "owners": config.owners,
            "sessions": len(targets),
            "works": len(targets),
            "read_attachments": sum(len(target.attachment_ids) for target in targets),
        },
        "invariants": invariants,
        "coverage": {
            "missing_read_samples": missing_read_coverage[:100],
            "missing_write_samples": missing_write_coverage[:100],
            "missing_read_sample_count": len(missing_read_coverage),
            "missing_write_sample_count": len(missing_write_coverage),
        },
        "ok": ok,
        "operations": summarize_operation_results(all_results),
        "metrics": metrics_summary,
        "limits": {
            "max_response_bytes": MAX_RESPONSE_BYTES,
            "query_history_bound": "not measured; rows-examined instrumentation is deployment-specific",
        },
        "scope": {
            "proves": [
                "public HTTP protocol owner isolation",
                "durable Work identity after mixed read/write pressure",
                "bounded public page responses",
                "idempotent Work creation retry",
            ],
            "does_not_prove": [
                "TUI or Web visual E2E",
                "LLM/provider quality or admission latency",
                "automatic Edge takeover or Run-owner recovery",
                "database rows examined without deployment instrumentation",
            ],
        },
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=os.environ.get("ASTRA_API_BASE_URL", "http://127.0.0.1:17001"))
    parser.add_argument("--profile", choices=sorted(PROFILE_DEFAULTS), default="smoke")
    parser.add_argument("--owners", type=int)
    parser.add_argument("--sessions-per-owner", type=int)
    parser.add_argument("--duration-secs", type=float)
    parser.add_argument("--read-rate", type=float)
    parser.add_argument("--write-rate", type=float)
    parser.add_argument("--metrics-interval-secs", type=float)
    parser.add_argument("--auth-token")
    parser.add_argument("--token-file")
    parser.add_argument("--register-users", action="store_true")
    parser.add_argument("--register-prefix", default="durable-work-pressure")
    parser.add_argument("--register-password", default="DurableWorkPressure123!")
    parser.add_argument("--register-domain", default="durable-work-pressure.local")
    parser.add_argument("--register-concurrency", type=int, default=8)
    parser.add_argument("--connect-timeout-secs", type=float, default=10.0)
    parser.add_argument("--request-timeout-secs", type=float, default=30.0)
    parser.add_argument("--slow-session-index", type=int, default=0)
    parser.add_argument("--slow-delay-secs", type=float, default=0.25)
    parser.add_argument("--require-metrics", action="store_true")
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--dry-run", action="store_true")
    return parser


def config_from_args(args: argparse.Namespace) -> ProbeConfig:
    defaults = PROFILE_DEFAULTS[args.profile]
    owners = positive_int(args.owners, int(defaults["owners"]), "owners")
    sessions = positive_int(
        args.sessions_per_owner,
        int(defaults["sessions_per_owner"]),
        "sessions-per-owner",
    )
    if sessions < 2:
        raise ProbeError("--sessions-per-owner must be at least 2 (one writer and one reader)")
    duration = positive_float(args.duration_secs, float(defaults["duration_secs"]), "duration-secs")
    read_rate = positive_float(args.read_rate, float(defaults["read_rate"]), "read-rate")
    write_rate = positive_float(args.write_rate, float(defaults["write_rate"]), "write-rate")
    metrics_interval = positive_float(
        args.metrics_interval_secs,
        float(defaults["metrics_interval_secs"]),
        "metrics-interval-secs",
    )
    if args.register_concurrency <= 0:
        raise ProbeError("--register-concurrency must be positive")
    if owners < 2:
        raise ProbeError("--owners must be at least 2 for the owner-isolation contract")
    if args.connect_timeout_secs <= 0 or args.request_timeout_secs <= 0:
        raise ProbeError("timeouts must be positive")
    if args.slow_session_index < 0 or args.slow_session_index >= owners * sessions:
        raise ProbeError("--slow-session-index must identify one configured session")
    if args.slow_delay_secs < 0:
        raise ProbeError("--slow-delay-secs must be non-negative")
    if duration * read_rate < len(READ_OPERATIONS):
        raise ProbeError(
            "duration × read-rate must allow at least one sample of every read operation"
        )
    if duration * write_rate < 1:
        raise ProbeError("duration × write-rate must allow at least one write cycle")
    output_dir = args.output_dir or Path("tmp") / "durable-work-pressure" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return ProbeConfig(
        base_url=args.base_url,
        owners=owners,
        sessions_per_owner=sessions,
        duration_secs=duration,
        read_rate=read_rate,
        write_rate=write_rate,
        metrics_interval_secs=metrics_interval,
        connect_timeout_secs=args.connect_timeout_secs,
        request_timeout_secs=args.request_timeout_secs,
        register_users=args.register_users,
        register_prefix=args.register_prefix,
        register_password=args.register_password,
        register_domain=args.register_domain,
        register_concurrency=args.register_concurrency,
        auth_tokens=tuple(parse_tokens(args.token_file, args.auth_token or os.environ.get("ASTRA_AUTH_TOKEN"))),
        output_dir=output_dir,
        slow_session_index=args.slow_session_index,
        slow_delay_secs=args.slow_delay_secs,
        require_metrics=args.require_metrics,
    )


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        config = config_from_args(args)
        if args.dry_run:
            print(
                json.dumps(
                    {
                        "profile": args.profile,
                        "owners": config.owners,
                        "sessions_per_owner": config.sessions_per_owner,
                        "duration_secs": config.duration_secs,
                        "read_rate": config.read_rate,
                        "write_rate": config.write_rate,
                        "slow_session_index": config.slow_session_index,
                        "dataset_works": config.total_sessions,
                        "dataset_read_attachments": config.total_sessions * 2,
                        "output_dir": str(config.output_dir),
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        config.output_dir.mkdir(parents=True, exist_ok=True)
        summary = asyncio.run(run_probe(config))
        summary_path = config.output_dir / "summary.json"
        summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps(summary, indent=2, sort_keys=True))
        print(f"wrote {summary_path}", file=sys.stderr)
        return 0 if summary["ok"] else 2
    except KeyboardInterrupt:
        return 130
    except ProbeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
