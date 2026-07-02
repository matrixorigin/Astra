#!/usr/bin/env python3
"""Run a concurrent /chat/stream capacity probe against astra runtime.

The probe intentionally uses only Python stdlib. Capacity debugging should not
depend on a second async HTTP stack or package manager state.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import re
import ssl
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


PROFILE_DEFAULTS = {
    "100-cli": {"concurrency": 100, "total": 100},
    "500-cli": {"concurrency": 500, "total": 500},
}

DEFAULT_METRIC_PREFIXES = (
    "astra_capacity_",
    "astra_run_admission_",
    "astra_tool_execution_",
    "astra_llm_provider_admission_",
    "astra_llm_provider_rate_limit_",
    "astra_llm_nonstream_fallback_",
    "astra_durable_run_event_",
    "astra_event_ingestion_",
    "astra_run_control_",
    "astra_ws_run_stream_",
    "astra_edge_dispatch_",
    "astra_edge_registry_",
    "astra_interaction_",
    "astra_task_lease_",
    "astra_multi_agent_",
    "astra_turn_observer_",
    "astra_post_loop_memory_cleanup_",
    "astra_session_memory_post_loop_",
)


class ProbeError(Exception):
    """Expected probe failure with a human-readable message."""


@dataclass(frozen=True)
class ParsedUrl:
    scheme: str
    host: str
    port: int
    target: str
    authority: str

    @property
    def use_tls(self) -> bool:
        return self.scheme == "https"


@dataclass
class HttpResponse:
    status: int
    reason: str
    headers: dict[str, str]
    body: bytes
    header_latency_ms: float


@dataclass
class StreamResult:
    request_id: int
    user_index: int
    token_index: int | None
    http_status: int | None
    header_latency_ms: float | None
    first_event_ms: float | None
    duration_ms: float
    event_count: int
    session_id: str | None
    run_id: str | None
    terminal_status: str | None
    error_code: str | None
    error_message: str | None
    retryable: bool | None
    outcome: str

    def to_json(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "user_index": self.user_index,
            "token_index": self.token_index,
            "http_status": self.http_status,
            "header_latency_ms": self.header_latency_ms,
            "first_event_ms": self.first_event_ms,
            "duration_ms": self.duration_ms,
            "event_count": self.event_count,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "terminal_status": self.terminal_status,
            "error_code": self.error_code,
            "error_message": self.error_message,
            "retryable": self.retryable,
            "outcome": self.outcome,
        }


class OutputWriter:
    def __init__(self, output_dir: Path) -> None:
        self.output_dir = output_dir
        self.output_dir.mkdir(parents=True, exist_ok=True)
        self.requests_path = self.output_dir / "requests.jsonl"
        self.metrics_path = self.output_dir / "metrics.jsonl"
        self.metrics_raw_path = self.output_dir / "metrics-snapshots.prom"
        self.summary_path = self.output_dir / "summary.json"
        self._lock = asyncio.Lock()
        self._requests_file = self.requests_path.open("a", encoding="utf-8", buffering=1)
        self._metrics_file = self.metrics_path.open("a", encoding="utf-8", buffering=1)
        self._metrics_raw_file = self.metrics_raw_path.open("a", encoding="utf-8", buffering=1)

    async def write_request(self, result: StreamResult) -> None:
        line = json.dumps(result.to_json(), sort_keys=True)
        async with self._lock:
            self._requests_file.write(line + "\n")

    async def write_metrics(self, sample: dict[str, Any], raw: str) -> None:
        line = json.dumps(sample, sort_keys=True)
        async with self._lock:
            self._metrics_file.write(line + "\n")
            self._metrics_raw_file.write(f"# sample_unix_ms {sample['unix_ms']}\n")
            self._metrics_raw_file.write(raw)
            if raw and not raw.endswith("\n"):
                self._metrics_raw_file.write("\n")

    def write_summary(self, summary: dict[str, Any]) -> None:
        with self.summary_path.open("w", encoding="utf-8") as f:
            json.dump(summary, f, indent=2, sort_keys=True)
            f.write("\n")

    def close(self) -> None:
        for handle in (self._requests_file, self._metrics_file, self._metrics_raw_file):
            handle.close()


def parse_url(base_url: str, path: str | None = None) -> ParsedUrl:
    base = urlsplit(base_url)
    if base.scheme not in ("http", "https"):
        raise ProbeError(f"unsupported URL scheme: {base.scheme!r}")
    if not base.hostname:
        raise ProbeError(f"missing host in URL: {base_url}")
    if path is None:
        path_part = base.path or "/"
    else:
        path_part = path if path.startswith("/") else f"/{path}"
    target = path_part
    if base.query:
        target = f"{target}?{base.query}"
    port = base.port or (443 if base.scheme == "https" else 80)
    default_port = 443 if base.scheme == "https" else 80
    authority = base.hostname if port == default_port else f"{base.hostname}:{port}"
    return ParsedUrl(base.scheme, base.hostname, port, target, authority)


def merge_base_url(base_url: str, path: str) -> str:
    base = base_url.rstrip("/")
    suffix = path if path.startswith("/") else f"/{path}"
    return f"{base}{suffix}"


async def open_http_connection(url: ParsedUrl, timeout_secs: float) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    ssl_context = ssl.create_default_context() if url.use_tls else None
    try:
        return await asyncio.wait_for(
            asyncio.open_connection(
                url.host,
                url.port,
                ssl=ssl_context,
                server_hostname=url.host if url.use_tls else None,
            ),
            timeout=timeout_secs,
        )
    except Exception as exc:  # noqa: BLE001
        raise ProbeError(f"connect failed for {url.host}:{url.port}: {exc}") from exc


async def read_until_headers(reader: asyncio.StreamReader, timeout_secs: float) -> bytes:
    chunks: list[bytes] = []
    total = 0
    deadline = time.monotonic() + timeout_secs
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProbeError("timed out waiting for response headers")
        chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
        if not chunk:
            raise ProbeError("connection closed before response headers")
        chunks.append(chunk)
        total += len(chunk)
        if total > 1024 * 1024:
            raise ProbeError("response headers exceeded 1 MiB")
        joined = b"".join(chunks)
        marker = joined.find(b"\r\n\r\n")
        if marker >= 0:
            return joined


def parse_headers(header_blob: bytes) -> tuple[int, str, dict[str, str], bytes]:
    marker = header_blob.find(b"\r\n\r\n")
    if marker < 0:
        raise ProbeError("response headers missing terminator")
    head = header_blob[:marker].decode("iso-8859-1")
    rest = header_blob[marker + 4 :]
    lines = head.split("\r\n")
    status_parts = lines[0].split(" ", 2)
    if len(status_parts) < 2 or not status_parts[1].isdigit():
        raise ProbeError(f"invalid HTTP status line: {lines[0]!r}")
    status = int(status_parts[1])
    reason = status_parts[2] if len(status_parts) > 2 else ""
    headers: dict[str, str] = {}
    for line in lines[1:]:
        if not line or ":" not in line:
            continue
        key, value = line.split(":", 1)
        headers[key.strip().lower()] = value.strip()
    return status, reason, headers, rest


async def read_chunked_body(
    reader: asyncio.StreamReader,
    initial: bytes,
    timeout_secs: float,
) -> bytes:
    data = bytearray()
    buffered = bytearray(initial)
    deadline = time.monotonic() + timeout_secs

    async def read_line() -> bytes:
        while True:
            pos = buffered.find(b"\r\n")
            if pos >= 0:
                line = bytes(buffered[:pos])
                del buffered[: pos + 2]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProbeError("timed out while reading chunked body")
            chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
            if not chunk:
                raise ProbeError("connection closed inside chunked body")
            buffered.extend(chunk)

    async def read_exact(count: int) -> bytes:
        while len(buffered) < count:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProbeError("timed out while reading chunked body")
            chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
            if not chunk:
                raise ProbeError("connection closed inside chunked body")
            buffered.extend(chunk)
        out = bytes(buffered[:count])
        del buffered[:count]
        return out

    while True:
        size_line = await read_line()
        size_text = size_line.split(b";", 1)[0].strip()
        if not size_text:
            continue
        try:
            size = int(size_text, 16)
        except ValueError as exc:
            raise ProbeError(f"invalid chunk size: {size_line!r}") from exc
        if size == 0:
            while await read_line():
                pass
            return bytes(data)
        data.extend(await read_exact(size))
        crlf = await read_exact(2)
        if crlf != b"\r\n":
            raise ProbeError("chunk data missing CRLF terminator")


async def body_chunks(
    reader: asyncio.StreamReader,
    headers: dict[str, str],
    initial: bytes,
    timeout_secs: float,
):
    transfer = headers.get("transfer-encoding", "").lower()
    deadline = time.monotonic() + timeout_secs
    if "chunked" in transfer:
        buffered = bytearray(initial)

        async def read_line() -> bytes:
            while True:
                pos = buffered.find(b"\r\n")
                if pos >= 0:
                    line = bytes(buffered[:pos])
                    del buffered[: pos + 2]
                    return line
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ProbeError("timed out while reading chunked stream")
                chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
                if not chunk:
                    raise ProbeError("connection closed inside chunked stream")
                buffered.extend(chunk)

        async def read_exact(count: int) -> bytes:
            while len(buffered) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ProbeError("timed out while reading chunked stream")
                chunk = await asyncio.wait_for(reader.read(4096), timeout=remaining)
                if not chunk:
                    raise ProbeError("connection closed inside chunked stream")
                buffered.extend(chunk)
            out = bytes(buffered[:count])
            del buffered[:count]
            return out

        while True:
            size_line = await read_line()
            size_text = size_line.split(b";", 1)[0].strip()
            if not size_text:
                continue
            size = int(size_text, 16)
            if size == 0:
                while await read_line():
                    pass
                return
            yield await read_exact(size)
            crlf = await read_exact(2)
            if crlf != b"\r\n":
                raise ProbeError("chunk data missing CRLF terminator")
        return

    content_length = headers.get("content-length")
    if content_length and content_length.isdigit():
        remaining_bytes = int(content_length)
        if initial:
            take = initial[:remaining_bytes]
            remaining_bytes -= len(take)
            if take:
                yield take
        while remaining_bytes > 0:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProbeError("timed out while reading body")
            chunk = await asyncio.wait_for(reader.read(min(65536, remaining_bytes)), timeout=remaining)
            if not chunk:
                break
            remaining_bytes -= len(chunk)
            yield chunk
        return

    if initial:
        yield initial
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProbeError("timed out while reading body")
        chunk = await asyncio.wait_for(reader.read(65536), timeout=remaining)
        if not chunk:
            return
        yield chunk


async def http_request(
    method: str,
    url_text: str,
    headers: dict[str, str],
    body: bytes | None,
    connect_timeout_secs: float,
    request_timeout_secs: float,
) -> HttpResponse:
    parsed = parse_url(url_text)
    reader, writer = await open_http_connection(parsed, connect_timeout_secs)
    started = time.perf_counter()
    try:
        request_headers = {
            "host": parsed.authority,
            "user-agent": "astra-capacity-probe/1",
            "accept": "*/*",
            "connection": "close",
        }
        request_headers.update({k.lower(): v for k, v in headers.items()})
        body = body or b""
        if body:
            request_headers.setdefault("content-type", "application/json")
        request_headers["content-length"] = str(len(body))
        lines = [f"{method.upper()} {parsed.target} HTTP/1.1"]
        lines.extend(f"{k}: {v}" for k, v in request_headers.items())
        raw = ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8") + body
        writer.write(raw)
        await writer.drain()
        header_blob = await read_until_headers(reader, request_timeout_secs)
        header_latency_ms = (time.perf_counter() - started) * 1000.0
        status, reason, response_headers, initial = parse_headers(header_blob)
        if "chunked" in response_headers.get("transfer-encoding", "").lower():
            response_body = await read_chunked_body(reader, initial, request_timeout_secs)
        else:
            chunks = []
            async for chunk in body_chunks(reader, response_headers, initial, request_timeout_secs):
                chunks.append(chunk)
            response_body = b"".join(chunks)
        return HttpResponse(status, reason, response_headers, response_body, header_latency_ms)
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:  # noqa: BLE001
            pass


async def stream_sse_request(
    request_id: int,
    user_index: int,
    token_index: int | None,
    url_text: str,
    headers: dict[str, str],
    body: bytes,
    connect_timeout_secs: float,
    request_timeout_secs: float,
) -> StreamResult:
    parsed = parse_url(url_text)
    started = time.perf_counter()
    http_status: int | None = None
    header_latency_ms: float | None = None
    first_event_ms: float | None = None
    event_count = 0
    session_id = None
    run_id = None
    terminal_status = None
    error_code = None
    error_message = None
    retryable = None
    outcome = "transport_error"
    reader: asyncio.StreamReader | None = None
    writer: asyncio.StreamWriter | None = None

    try:
        reader, writer = await open_http_connection(parsed, connect_timeout_secs)
        request_headers = {
            "host": parsed.authority,
            "user-agent": "astra-capacity-probe/1",
            "accept": "text/event-stream",
            "content-type": "application/json",
            "connection": "close",
            "content-length": str(len(body)),
        }
        request_headers.update({k.lower(): v for k, v in headers.items()})
        lines = [f"POST {parsed.target} HTTP/1.1"]
        lines.extend(f"{k}: {v}" for k, v in request_headers.items())
        writer.write(("\r\n".join(lines) + "\r\n\r\n").encode("utf-8") + body)
        await writer.drain()
        header_blob = await read_until_headers(reader, request_timeout_secs)
        header_latency_ms = (time.perf_counter() - started) * 1000.0
        http_status, _reason, response_headers, initial = parse_headers(header_blob)
        parser = SseParser()
        capture_http_error_body = not (200 <= http_status < 300)
        http_error_body_parts: list[str] = []
        http_error_body_chars = 0
        async for chunk in body_chunks(reader, response_headers, initial, request_timeout_secs):
            text = chunk.decode("utf-8", errors="replace")
            if capture_http_error_body and http_error_body_chars < 65536:
                remaining_chars = 65536 - http_error_body_chars
                http_error_body_parts.append(text[:remaining_chars])
                http_error_body_chars += min(len(text), remaining_chars)
            for event in parser.feed(text):
                event_count += 1
                if first_event_ms is None:
                    first_event_ms = (time.perf_counter() - started) * 1000.0
                event_type = event.get("type")
                if event_type == "session_info":
                    session_id = string_or_none(event.get("session_id")) or session_id
                    run_id = string_or_none(event.get("run_id")) or run_id
                elif event_type in ("error", "run_error"):
                    error_code = error_code_from_event(event) or error_code
                    error_message = error_message_from_event(event) or error_message
                    retryable = bool(event.get("retryable")) if "retryable" in event else None
                    if event_type == "error":
                        outcome = "sse_error"
                elif event_type == "run_finished":
                    terminal_status = string_or_none(event.get("status"))
                    error_code = error_code or error_code_from_event(event)
                    error_message = error_message or error_message_from_event(event)
                    if terminal_status in ("completed", "succeeded", "success", "ok"):
                        outcome = "completed"
                    else:
                        outcome = "terminal_non_success"
                    break
            if terminal_status is not None or outcome == "sse_error":
                break
        if outcome == "transport_error":
            if http_status and 200 <= http_status < 300:
                outcome = "eof_without_terminal"
            else:
                outcome = "http_error"
                body_code, body_message, body_retryable = error_details_from_http_body(
                    "".join(http_error_body_parts)
                )
                error_code = error_code or body_code
                error_message = error_message or body_message
                retryable = retryable if retryable is not None else body_retryable
    except asyncio.TimeoutError:
        outcome = "timeout"
        error_message = "request timed out"
    except Exception as exc:  # noqa: BLE001
        outcome = "transport_error"
        error_message = str(exc)
    finally:
        if writer is not None:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:  # noqa: BLE001
                pass

    return StreamResult(
        request_id=request_id,
        user_index=user_index,
        token_index=token_index,
        http_status=http_status,
        header_latency_ms=round_or_none(header_latency_ms),
        first_event_ms=round_or_none(first_event_ms),
        duration_ms=round((time.perf_counter() - started) * 1000.0, 3),
        event_count=event_count,
        session_id=session_id,
        run_id=run_id,
        terminal_status=terminal_status,
        error_code=error_code,
        error_message=error_message,
        retryable=retryable,
        outcome=outcome,
    )


class SseParser:
    def __init__(self) -> None:
        self.buffer = ""

    def feed(self, text: str) -> list[dict[str, Any]]:
        self.buffer += text
        events: list[dict[str, Any]] = []
        while "\n\n" in self.buffer:
            frame, self.buffer = self.buffer.split("\n\n", 1)
            event = parse_sse_frame(frame)
            if event is not None:
                events.append(event)
        return events


def parse_sse_frame(frame: str) -> dict[str, Any] | None:
    data_lines = []
    for line in frame.splitlines():
        if line.startswith(":") or not line:
            continue
        if line.startswith("data:"):
            data_lines.append(line[5:].lstrip())
    if not data_lines:
        return None
    data = "\n".join(data_lines)
    try:
        parsed = json.loads(data)
    except json.JSONDecodeError:
        return {"type": "malformed", "raw": data}
    return parsed if isinstance(parsed, dict) else {"type": "data", "value": parsed}


def parse_sse_events_from_text(text: str) -> list[dict[str, Any]]:
    parser = SseParser()
    return parser.feed(text + ("\n\n" if not text.endswith("\n\n") else ""))


METRIC_LINE_RE = re.compile(
    r"^(?P<name>[a-zA-Z_:][a-zA-Z0-9_:]*)(?P<labels>\{[^}]*\})?\s+(?P<value>[-+0-9.eE]+)"
)

EVENT_INGESTION_METRIC_KEYS = {
    "enqueue_overflows_total": "astra_event_ingestion_enqueue_overflows_total",
    "dropped_before_acceptance_total": "astra_event_ingestion_events_dropped_before_acceptance_total",
    "dropped_before_acceptance_critical_total": (
        'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="critical"}'
    ),
    "dropped_before_acceptance_telemetry_total": (
        'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="telemetry"}'
    ),
    "dropped_permanent_total": "astra_event_ingestion_events_dropped_permanent_total",
    "errors_total": "astra_event_ingestion_errors_total",
}

RUN_CONTROL_ATTEMPTS_METRIC = "astra_run_control_poll_attempts_total"
RUN_CONTROL_ERRORS_METRIC = "astra_run_control_poll_errors_total"
WS_RUN_STREAM_ATTEMPTS_METRIC = "astra_ws_run_stream_poll_attempts_total"
WS_RUN_STREAM_ERRORS_METRIC = "astra_ws_run_stream_poll_errors_total"
METRIC_KEY_RE = re.compile(r"^(?P<name>[^{]+)(?:\{(?P<labels>.*)\})?$")
METRIC_LABEL_RE = re.compile(r'(?P<key>[a-zA-Z_][a-zA-Z0-9_]*)="(?P<value>(?:\\.|[^"])*)"')


def parse_prometheus_metrics(text: str, prefixes: tuple[str, ...] = DEFAULT_METRIC_PREFIXES) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        match = METRIC_LINE_RE.match(line)
        if not match:
            continue
        name = match.group("name")
        if prefixes and not any(name.startswith(prefix) for prefix in prefixes):
            continue
        labels = match.group("labels") or ""
        try:
            value = float(match.group("value"))
        except ValueError:
            continue
        metrics[f"{name}{labels}"] = value
    return metrics


def split_metric_key(key: str) -> tuple[str, dict[str, str]]:
    match = METRIC_KEY_RE.match(key)
    if not match:
        return key, {}
    labels_text = match.group("labels") or ""
    labels = {
        item.group("key"): item.group("value").replace(r"\"", '"').replace(r"\\", "\\")
        for item in METRIC_LABEL_RE.finditer(labels_text)
    }
    return match.group("name"), labels


def render_template(value: Any, mapping: dict[str, str]) -> Any:
    if isinstance(value, str):
        out = value
        for key, replacement in mapping.items():
            out = out.replace("{" + key + "}", replacement)
        return out
    if isinstance(value, list):
        return [render_template(item, mapping) for item in value]
    if isinstance(value, dict):
        return {str(k): render_template(v, mapping) for k, v in value.items()}
    return value


def default_body(args: argparse.Namespace, request_id: int, user_index: int) -> dict[str, Any]:
    message = args.message.format(request_id=request_id, user_index=user_index, profile=args.profile)
    body: dict[str, Any] = {
        "message": message,
        "context": {
            "capacity_probe": {
                "profile": args.profile,
                "request_id": request_id,
                "user_index": user_index,
            }
        },
    }
    if args.agent_id:
        body["agent_id"] = args.agent_id
    if args.model:
        body["selected_model"] = {"model": args.model}
    if args.session_id_template:
        body["session_id"] = args.session_id_template.format(
            request_id=request_id,
            user_index=user_index,
            profile=args.profile,
        )
    return body


def load_body_template(path: str | None) -> Any:
    if not path:
        return None
    with Path(path).open("r", encoding="utf-8") as f:
        return json.load(f)


def body_for_request(args: argparse.Namespace, template: Any, request_id: int, user_index: int) -> bytes:
    if template is None:
        body = default_body(args, request_id, user_index)
    else:
        mapping = {
            "request_id": str(request_id),
            "user_index": str(user_index),
            "profile": args.profile,
            "message": args.message.format(
                request_id=request_id,
                user_index=user_index,
                profile=args.profile,
            ),
        }
        body = render_template(template, mapping)
    return json.dumps(body, separators=(",", ":"), sort_keys=True).encode("utf-8")


def body_has_selected_model(body: Any) -> bool:
    if not isinstance(body, dict):
        return False
    selected_model = body.get("selected_model")
    if not isinstance(selected_model, dict):
        return False
    model = selected_model.get("model")
    return isinstance(model, str) and bool(model.strip())


def validate_stream_body_contract(args: argparse.Namespace, template: Any) -> None:
    if args.model:
        return
    sample = json.loads(body_for_request(args, template, 0, 0).decode("utf-8"))
    if body_has_selected_model(sample):
        return
    raise ProbeError(
        "/chat/stream requires selected_model.model; pass --model or include "
        "selected_model.model in --body-template"
    )


def load_tokens(path: str | None, explicit_token: str | None) -> list[str]:
    tokens: list[str] = []
    if explicit_token:
        tokens.append(strip_bearer(explicit_token))
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
                        tokens.append(strip_bearer(item))
                    elif isinstance(item, dict) and isinstance(item.get("access_token"), str):
                        tokens.append(strip_bearer(item["access_token"]))
            elif isinstance(parsed, dict):
                values = parsed.get("tokens", parsed.get("access_tokens", []))
                if isinstance(values, list):
                    for item in values:
                        if isinstance(item, str):
                            tokens.append(strip_bearer(item))
                elif isinstance(parsed.get("access_token"), str):
                    tokens.append(strip_bearer(parsed["access_token"]))
            else:
                for line in raw.splitlines():
                    line = line.strip()
                    if line and not line.startswith("#"):
                        tokens.append(strip_bearer(line))
    deduped: list[str] = []
    seen = set()
    for token in tokens:
        if token and token not in seen:
            seen.add(token)
            deduped.append(token)
    return deduped


def strip_bearer(token: str) -> str:
    token = token.strip()
    if token.lower().startswith("bearer "):
        return token.split(None, 1)[1].strip()
    return token


async def register_or_login_user(args: argparse.Namespace, index: int) -> str:
    username = f"{args.register_prefix}-{index:05d}"
    password = args.register_password
    email = f"{username}@{args.register_domain}"
    register_url = merge_base_url(args.base_url, "/auth/register")
    login_url = merge_base_url(args.base_url, "/auth/login")
    body = json.dumps(
        {
            "username": username,
            "email": email,
            "password": password,
            "display_name": username,
        },
        separators=(",", ":"),
    ).encode("utf-8")
    headers = {"content-type": "application/json"}
    response = await http_request(
        "POST",
        register_url,
        headers,
        body,
        args.connect_timeout_secs,
        args.request_timeout_secs,
    )
    if response.status not in range(200, 300):
        login_body = json.dumps(
            {"username": username, "password": password},
            separators=(",", ":"),
        ).encode("utf-8")
        response = await http_request(
            "POST",
            login_url,
            headers,
            login_body,
            args.connect_timeout_secs,
            args.request_timeout_secs,
        )
    if response.status not in range(200, 300):
        raise ProbeError(f"auth bootstrap failed for {username}: HTTP {response.status} {response.body[:200]!r}")
    try:
        parsed = json.loads(response.body.decode("utf-8"))
    except json.JSONDecodeError as exc:
        raise ProbeError(f"auth bootstrap returned non-JSON for {username}") from exc
    token = parsed.get("access_token")
    if not isinstance(token, str) or not token:
        raise ProbeError(f"auth bootstrap response missing access_token for {username}")
    return strip_bearer(token)


async def bootstrap_tokens(args: argparse.Namespace, requested_count: int) -> list[str]:
    tokens = load_tokens(args.token_file, args.auth_token or os.environ.get("ASTRA_AUTH_TOKEN"))
    if not args.register_users:
        return tokens
    semaphore = asyncio.Semaphore(args.register_concurrency)
    results: list[str | None] = [None] * requested_count

    async def worker(index: int) -> None:
        async with semaphore:
            results[index] = await register_or_login_user(args, index)

    await asyncio.gather(*(worker(i) for i in range(requested_count)))
    generated = [token for token in results if token]
    token_output = args.output_dir / "registered-tokens.json"
    token_output.parent.mkdir(parents=True, exist_ok=True)
    token_output.write_text(
        json.dumps({"tokens": generated}, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return generated


async def metrics_sampler(
    args: argparse.Namespace,
    writer: OutputWriter,
    stop_event: asyncio.Event,
) -> None:
    metrics_url = merge_base_url(args.base_url, args.metrics_path)
    headers = {}
    if args.metrics_auth_token:
        headers["authorization"] = f"Bearer {strip_bearer(args.metrics_auth_token)}"
    while not stop_event.is_set():
        unix_ms = int(time.time() * 1000)
        try:
            response = await http_request(
                "GET",
                metrics_url,
                headers,
                None,
                args.connect_timeout_secs,
                min(args.request_timeout_secs, 30),
            )
            raw = response.body.decode("utf-8", errors="replace")
            sample = {
                "unix_ms": unix_ms,
                "http_status": response.status,
                "metrics": parse_prometheus_metrics(raw),
            }
            await writer.write_metrics(sample, raw)
        except Exception as exc:  # noqa: BLE001
            await writer.write_metrics(
                {
                    "unix_ms": unix_ms,
                    "http_status": None,
                    "error": str(exc),
                    "metrics": {},
                },
                "",
            )
        try:
            await asyncio.wait_for(stop_event.wait(), timeout=args.metrics_interval_secs)
        except asyncio.TimeoutError:
            pass


async def run_probe(args: argparse.Namespace) -> int:
    defaults = PROFILE_DEFAULTS[args.profile]
    args.concurrency = args.concurrency or defaults["concurrency"]
    args.total = args.total or defaults["total"]
    args.output_dir = args.output_dir or default_output_dir(args.profile)
    template = load_body_template(args.body_template)
    if not args.dry_run:
        validate_stream_body_contract(args, template)

    if args.dry_run:
        nofile_soft_limit = current_nofile_soft_limit()
        nofile_required = estimated_nofile_required(args.concurrency)
        print(
            json.dumps(
                {
                    "base_url": args.base_url,
                    "endpoint": args.endpoint,
                    "profile": args.profile,
                    "concurrency": args.concurrency,
                    "total": args.total,
                    "register_users": args.register_users,
                    "require_metrics": args.require_metrics,
                    "require_error_codes_for_failures": args.require_error_codes_for_failures,
                    "require_no_critical_ingestion_drops": args.require_no_critical_ingestion_drops,
                    "nofile": {
                        "soft_limit": nofile_soft_limit,
                        "required": nofile_required,
                        "sufficient": nofile_soft_limit is None or nofile_soft_limit >= nofile_required,
                    },
                    "output_dir": str(args.output_dir),
                    "body_example": json.loads(body_for_request(args, template, 0, 0).decode("utf-8")),
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0

    writer = OutputWriter(args.output_dir)
    try:
        tokens = await bootstrap_tokens(args, args.concurrency if args.register_users else 0)
        if not tokens:
            raise ProbeError("no auth token available; pass --auth-token, --token-file, or --register-users")
        if args.require_distinct_users and len(tokens) < args.concurrency:
            raise ProbeError(
                f"--require-distinct-users needs at least concurrency tokens; have {len(tokens)}, need {args.concurrency}"
            )
        if len(tokens) < args.concurrency:
            print(
                f"warning: {len(tokens)} token(s) for concurrency={args.concurrency}; tokens will be reused",
                file=sys.stderr,
            )

        stream_url = merge_base_url(args.base_url, args.endpoint)
        queue: asyncio.Queue[int] = asyncio.Queue()
        for request_id in range(args.total):
            queue.put_nowait(request_id)
        results: list[StreamResult] = []
        results_lock = asyncio.Lock()
        stop_metrics = asyncio.Event()
        sampler = asyncio.create_task(metrics_sampler(args, writer, stop_metrics))

        async def worker(worker_id: int) -> None:
            while True:
                try:
                    request_id = queue.get_nowait()
                except asyncio.QueueEmpty:
                    return
                user_index = worker_id if args.user_mode == "worker" else request_id
                token_index = user_index % len(tokens)
                token = tokens[token_index]
                headers = {"authorization": f"Bearer {token}"}
                body = body_for_request(args, template, request_id, user_index)
                result = await stream_sse_request(
                    request_id=request_id,
                    user_index=user_index,
                    token_index=token_index,
                    url_text=stream_url,
                    headers=headers,
                    body=body,
                    connect_timeout_secs=args.connect_timeout_secs,
                    request_timeout_secs=args.request_timeout_secs,
                )
                await writer.write_request(result)
                async with results_lock:
                    results.append(result)
                    if len(results) % max(1, args.progress_every) == 0 or len(results) == args.total:
                        print_progress(results, args.total)
                queue.task_done()

        started_unix_ms = int(time.time() * 1000)
        started = time.perf_counter()
        await asyncio.gather(*(worker(i) for i in range(args.concurrency)))
        stop_metrics.set()
        await sampler
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        metrics_summary = summarize_metrics_file(writer.metrics_path)
        contract_violations: list[str] = []
        if args.require_metrics and metrics_summary["samples_with_metrics"] == 0:
            contract_violations.append("metrics_required_but_no_prefixed_metrics_sampled")
        critical_ingestion_drops = metrics_summary.get("event_ingestion", {}).get(
            "dropped_before_acceptance_critical_total"
        )
        if args.require_no_critical_ingestion_drops and critical_ingestion_drops is None:
            contract_violations.append("critical_ingestion_drop_metric_missing")
        elif args.require_no_critical_ingestion_drops and critical_ingestion_drops > 0:
            contract_violations.append(
                f"critical_ingestion_drops_before_acceptance:{critical_ingestion_drops:g}"
            )
        failures_missing_error_code = sum(
            1 for result in results if result.outcome != "completed" and not result.error_code
        )
        if args.require_error_codes_for_failures and failures_missing_error_code > 0:
            contract_violations.append(f"failures_missing_error_code:{failures_missing_error_code}")
        summary = summarize_results(
            results,
            args,
            started_unix_ms,
            int(time.time() * 1000),
            elapsed_ms,
            metrics_summary,
            contract_violations,
        )
        writer.write_summary(summary)
        print(json.dumps(summary, indent=2, sort_keys=True))
        return 0 if summary["failed"] == 0 and not contract_violations else 2
    finally:
        writer.close()


def print_progress(results: list[StreamResult], total: int) -> None:
    counts = counts_by([r.outcome for r in results])
    completed = len(results)
    print(f"progress {completed}/{total} outcomes={json.dumps(counts, sort_keys=True)}", file=sys.stderr)


def summarize_results(
    results: list[StreamResult],
    args: argparse.Namespace,
    started_unix_ms: int,
    ended_unix_ms: int,
    elapsed_ms: float,
    metrics_summary: dict[str, Any],
    contract_violations: list[str],
) -> dict[str, Any]:
    durations = [r.duration_ms for r in results]
    first_events = [r.first_event_ms for r in results if r.first_event_ms is not None]
    header_latencies = [r.header_latency_ms for r in results if r.header_latency_ms is not None]
    outcomes = counts_by([r.outcome for r in results])
    error_codes = counts_by([r.error_code or "none" for r in results if r.outcome != "completed"])
    terminal_statuses = counts_by([r.terminal_status or "none" for r in results])
    failure_reasons = counts_by([failure_reason(r) for r in results if r.outcome != "completed"])
    failed = sum(1 for r in results if r.outcome != "completed")
    failures_missing_error_code = sum(
        1 for r in results if r.outcome != "completed" and not r.error_code
    )
    return {
        "profile": args.profile,
        "base_url": args.base_url,
        "endpoint": args.endpoint,
        "concurrency": args.concurrency,
        "total": args.total,
        "started_unix_ms": started_unix_ms,
        "ended_unix_ms": ended_unix_ms,
        "elapsed_ms": round(elapsed_ms, 3),
        "throughput_rps": round(len(results) / (elapsed_ms / 1000.0), 3) if elapsed_ms > 0 else 0,
        "completed": outcomes.get("completed", 0),
        "failed": failed,
        "failures_missing_error_code": failures_missing_error_code,
        "outcomes": outcomes,
        "http_status": counts_by([str(r.http_status or "none") for r in results]),
        "error_codes": error_codes,
        "terminal_statuses": terminal_statuses,
        "failure_reasons": failure_reasons,
        "duration_ms": percentile_summary(durations),
        "first_event_ms": percentile_summary(first_events),
        "header_latency_ms": percentile_summary(header_latencies),
        "metrics": metrics_summary,
        "contract_violations": contract_violations,
        "output_dir": str(args.output_dir),
    }


def failure_reason(result: StreamResult) -> str:
    if result.error_code:
        return f"error_code:{result.error_code}"
    if result.terminal_status:
        return f"terminal_status:{result.terminal_status}"
    return f"outcome:{result.outcome}"


def summarize_metrics_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {
            "sample_count": 0,
            "samples_with_metrics": 0,
            "http_status": {},
            "errors": 0,
            "first_unix_ms": None,
            "last_unix_ms": None,
            "last_metric_count": 0,
            "last_metric_names": [],
            "event_ingestion": summarize_event_ingestion_metrics({}),
            "run_control": summarize_run_control_metrics({}, {}, None),
            "ws_run_stream": summarize_ws_run_stream_metrics({}, {}, None),
        }
    sample_count = 0
    samples_with_metrics = 0
    errors = 0
    statuses: list[str] = []
    first_unix_ms = None
    last_unix_ms = None
    first_metrics: dict[str, float] = {}
    last_metrics: dict[str, float] = {}
    with path.open("r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            sample_count += 1
            try:
                sample = json.loads(line)
            except json.JSONDecodeError:
                errors += 1
                continue
            unix_ms = sample.get("unix_ms")
            if isinstance(unix_ms, int):
                first_unix_ms = unix_ms if first_unix_ms is None else min(first_unix_ms, unix_ms)
                last_unix_ms = unix_ms if last_unix_ms is None else max(last_unix_ms, unix_ms)
            statuses.append(str(sample.get("http_status", "none")))
            if sample.get("error"):
                errors += 1
            metrics = sample.get("metrics")
            if isinstance(metrics, dict) and metrics:
                samples_with_metrics += 1
                if not first_metrics:
                    first_metrics = metrics
                last_metrics = metrics
    elapsed_ms = (
        last_unix_ms - first_unix_ms
        if isinstance(first_unix_ms, int) and isinstance(last_unix_ms, int)
        else None
    )
    return {
        "sample_count": sample_count,
        "samples_with_metrics": samples_with_metrics,
        "http_status": counts_by(statuses),
        "errors": errors,
        "first_unix_ms": first_unix_ms,
        "last_unix_ms": last_unix_ms,
        "last_metric_count": len(last_metrics),
        "last_metric_names": sorted(last_metrics)[:50],
        "event_ingestion": summarize_event_ingestion_metrics(last_metrics),
        "run_control": summarize_run_control_metrics(first_metrics, last_metrics, elapsed_ms),
        "ws_run_stream": summarize_ws_run_stream_metrics(first_metrics, last_metrics, elapsed_ms),
    }


def summarize_event_ingestion_metrics(metrics: dict[str, float]) -> dict[str, float | None]:
    return {name: metrics.get(key) for name, key in EVENT_INGESTION_METRIC_KEYS.items()}


def counter_delta(first: float | None, last: float | None) -> float | None:
    if last is None:
        return None
    if first is None:
        return last
    if last < first:
        return last
    return last - first


def summarize_counter_family(
    first_metrics: dict[str, float],
    last_metrics: dict[str, float],
    metric_name: str,
    label_fields: tuple[str, str],
) -> tuple[float | None, float | None, dict[str, dict[str, float | None]]]:
    keys = sorted(
        key
        for key in set(first_metrics) | set(last_metrics)
        if split_metric_key(key)[0] == metric_name
    )
    last_total = 0.0
    delta_total = 0.0
    saw_last = False
    saw_delta = False
    by_label: dict[str, dict[str, float | None]] = {}
    for key in keys:
        _, labels = split_metric_key(key)
        label_key = ":".join(labels.get(field, "none") for field in label_fields)
        first = first_metrics.get(key)
        last = last_metrics.get(key)
        delta = counter_delta(first, last)
        if last is not None:
            saw_last = True
            last_total += last
        if delta is not None:
            saw_delta = True
            delta_total += delta
        by_label[label_key] = {
            "last": last,
            "delta": delta,
        }
    return (
        last_total if saw_last else None,
        delta_total if saw_delta else None,
        by_label,
    )


def rate_per_sec(delta: float | None, elapsed_ms: int | None) -> float | None:
    if delta is None or not elapsed_ms or elapsed_ms <= 0:
        return None
    return round(delta / (elapsed_ms / 1000.0), 3)


def summarize_run_control_metrics(
    first_metrics: dict[str, float],
    last_metrics: dict[str, float],
    elapsed_ms: int | None,
) -> dict[str, Any]:
    attempt_last, attempt_delta, attempts = summarize_counter_family(
        first_metrics,
        last_metrics,
        RUN_CONTROL_ATTEMPTS_METRIC,
        ("operation", "outcome"),
    )
    error_last, error_delta, errors = summarize_counter_family(
        first_metrics,
        last_metrics,
        RUN_CONTROL_ERRORS_METRIC,
        ("operation", "class"),
    )
    return {
        "attempts_last_total": attempt_last,
        "attempts_delta_total": attempt_delta,
        "attempts_per_sec": rate_per_sec(attempt_delta, elapsed_ms),
        "attempts_by_operation_outcome": attempts,
        "errors_last_total": error_last,
        "errors_delta_total": error_delta,
        "errors_per_sec": rate_per_sec(error_delta, elapsed_ms),
        "errors_by_operation_class": errors,
    }


def summarize_ws_run_stream_metrics(
    first_metrics: dict[str, float],
    last_metrics: dict[str, float],
    elapsed_ms: int | None,
) -> dict[str, Any]:
    attempt_last, attempt_delta, attempts = summarize_counter_family(
        first_metrics,
        last_metrics,
        WS_RUN_STREAM_ATTEMPTS_METRIC,
        ("operation", "outcome"),
    )
    error_last, error_delta, errors = summarize_counter_family(
        first_metrics,
        last_metrics,
        WS_RUN_STREAM_ERRORS_METRIC,
        ("operation", "class"),
    )
    return {
        "attempts_last_total": attempt_last,
        "attempts_delta_total": attempt_delta,
        "attempts_per_sec": rate_per_sec(attempt_delta, elapsed_ms),
        "attempts_by_operation_outcome": attempts,
        "errors_last_total": error_last,
        "errors_delta_total": error_delta,
        "errors_per_sec": rate_per_sec(error_delta, elapsed_ms),
        "errors_by_operation_class": errors,
    }


def percentile_summary(values: list[float]) -> dict[str, float | None]:
    if not values:
        return {"min": None, "p50": None, "p95": None, "p99": None, "max": None}
    ordered = sorted(values)
    return {
        "min": round(ordered[0], 3),
        "p50": round(percentile(ordered, 50), 3),
        "p95": round(percentile(ordered, 95), 3),
        "p99": round(percentile(ordered, 99), 3),
        "max": round(ordered[-1], 3),
    }


def percentile(ordered: list[float], pct: float) -> float:
    if not ordered:
        raise ValueError("percentile requires values")
    if len(ordered) == 1:
        return ordered[0]
    rank = (pct / 100.0) * (len(ordered) - 1)
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return ordered[int(rank)]
    weight = rank - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def counts_by(values: list[str]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for value in values:
        counts[value] = counts.get(value, 0) + 1
    return dict(sorted(counts.items()))


def string_or_none(value: Any) -> str | None:
    return value if isinstance(value, str) else None


def error_code_from_event(event: dict[str, Any]) -> str | None:
    return string_or_none(event.get("error_code")) or string_or_none(event.get("code"))


def error_message_from_event(event: dict[str, Any]) -> str | None:
    return string_or_none(event.get("message")) or string_or_none(event.get("error"))


def error_details_from_http_body(body: str) -> tuple[str | None, str | None, bool | None]:
    body = body.strip()
    if not body:
        return None, None, None
    try:
        parsed = json.loads(body)
    except json.JSONDecodeError:
        return None, body[:500], None
    if not isinstance(parsed, dict):
        return None, None, None

    nested_error = parsed.get("error")
    nested = nested_error if isinstance(nested_error, dict) else {}
    code = (
        string_or_none(parsed.get("error_code"))
        or string_or_none(parsed.get("code"))
        or string_or_none(nested.get("error_code"))
        or string_or_none(nested.get("code"))
    )
    message = (
        string_or_none(parsed.get("detail"))
        or string_or_none(parsed.get("message"))
        or string_or_none(parsed.get("error"))
        or string_or_none(nested.get("detail"))
        or string_or_none(nested.get("message"))
    )
    retryable_value = parsed.get("retryable", nested.get("retryable"))
    retryable = retryable_value if isinstance(retryable_value, bool) else None
    return code, message, retryable


def round_or_none(value: float | None) -> float | None:
    return None if value is None else round(value, 3)


def default_output_dir(profile: str) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return Path("tmp") / "capacity-probe" / f"{stamp}-{profile}"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default=os.environ.get("ASTRA_API_BASE_URL", "http://127.0.0.1:17001"))
    parser.add_argument("--endpoint", default="/chat/stream")
    parser.add_argument("--metrics-path", default="/metrics")
    parser.add_argument("--profile", choices=sorted(PROFILE_DEFAULTS), default="100-cli")
    parser.add_argument("--concurrency", type=int)
    parser.add_argument("--total", type=int)
    parser.add_argument("--auth-token")
    parser.add_argument("--metrics-auth-token")
    parser.add_argument("--token-file")
    parser.add_argument("--register-users", action="store_true")
    parser.add_argument("--register-prefix", default="capacity-probe")
    parser.add_argument("--register-password", default="CapacityProbe123!")
    parser.add_argument("--register-domain", default="capacity-probe.local")
    parser.add_argument("--register-concurrency", type=int, default=20)
    parser.add_argument("--require-distinct-users", action="store_true")
    parser.add_argument("--user-mode", choices=("worker", "request"), default="worker")
    parser.add_argument("--model")
    parser.add_argument("--agent-id")
    parser.add_argument("--session-id-template")
    parser.add_argument(
        "--message",
        default="capacity probe request {request_id} for {profile}; answer with a short sentence",
    )
    parser.add_argument("--body-template", help="JSON file with {request_id}, {user_index}, {profile}, {message} placeholders")
    parser.add_argument("--connect-timeout-secs", type=float, default=10.0)
    parser.add_argument("--request-timeout-secs", type=float, default=300.0)
    parser.add_argument("--metrics-interval-secs", type=float, default=5.0)
    parser.add_argument(
        "--require-metrics",
        action="store_true",
        help="fail if /metrics did not yield any metrics matching the capacity probe prefixes",
    )
    parser.add_argument(
        "--require-error-codes-for-failures",
        action="store_true",
        help="fail if any non-completed request lacks a machine-readable error_code",
    )
    parser.add_argument(
        "--require-no-critical-ingestion-drops",
        action="store_true",
        help="fail if /metrics reports any critical agent_events ingestion drop before worker acceptance",
    )
    parser.add_argument(
        "--skip-nofile-check",
        action="store_true",
        help="skip local file-descriptor limit preflight; useful only when a wrapper raises limits after validation",
    )
    parser.add_argument("--progress-every", type=int, default=10)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--dry-run", action="store_true")
    return parser


def validate_args(args: argparse.Namespace) -> None:
    defaults = PROFILE_DEFAULTS[args.profile]
    concurrency = args.concurrency or defaults["concurrency"]
    total = args.total or defaults["total"]
    if concurrency <= 0:
        raise ProbeError("--concurrency must be positive")
    if total <= 0:
        raise ProbeError("--total must be positive")
    if args.register_concurrency <= 0:
        raise ProbeError("--register-concurrency must be positive")
    if args.connect_timeout_secs <= 0 or args.request_timeout_secs <= 0:
        raise ProbeError("timeouts must be positive")
    if args.metrics_interval_secs <= 0:
        raise ProbeError("--metrics-interval-secs must be positive")
    if not args.dry_run and not args.skip_nofile_check:
        validate_nofile_capacity(concurrency)


def validate_nofile_capacity(concurrency: int) -> None:
    soft_limit = current_nofile_soft_limit()
    if soft_limit is None:
        return
    required = estimated_nofile_required(concurrency)
    if soft_limit < required:
        raise ProbeError(
            "file descriptor limit too low for this probe: "
            f"soft={soft_limit}, required>={required}, concurrency={concurrency}. "
            "Run the API server and probe from shells with a higher limit, for example: "
            "ulimit -n 4096"
        )


def estimated_nofile_required(concurrency: int) -> int:
    # Each in-flight SSE request owns one client socket. Keep explicit headroom
    # for auth/metrics sockets, persistent output files, DNS/proxy internals,
    # and Python/runtime bookkeeping so the probe fails before EMFILE noise.
    return concurrency + max(64, math.ceil(concurrency * 0.1))


def current_nofile_soft_limit() -> int | None:
    try:
        import resource
    except ImportError:
        return None
    soft, _hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft < 0:
        return None
    return int(soft)


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        validate_args(args)
        return asyncio.run(run_probe(args))
    except KeyboardInterrupt:
        return 130
    except ProbeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
