#!/usr/bin/env python3
"""Run a local OpenAI-compatible mock server for capacity probes.

This is intentionally stdlib-only. It lets high-concurrency probes exercise the
real astra API/runtime/SSE/DB path without consuming external LLM provider quota.

Typical flow:

  python3 scripts/load/mock_openai_server.py --port 18080 \
    --model-name capacity-mock \
    --write-model-yaml tmp/capacity-mock-model.yaml

  ./rust/target/debug/astra admin model load tmp/capacity-mock-model.yaml --update-existing
  python3 scripts/load/multi_cli_capacity_probe.py --profile 500-cli \
    --model capacity-mock --register-users --require-distinct-users \
    --require-metrics --require-error-codes-for-failures
"""

from __future__ import annotations

import argparse
import json
import signal
import sys
import threading
import time
from dataclasses import dataclass, field
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlparse


COMPLETIONS_PATHS = {"/chat/completions", "/v1/chat/completions"}
DEFAULT_NOFILE_TARGET = 4096


class CapacityMockOpenAiServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 1024


@dataclass(frozen=True)
class MockConfig:
    model_name: str
    response_content: str
    prompt_tokens: int
    completion_tokens: int
    delay_ms: int
    status: int
    error_code: str
    error_message: str
    api_key: str


@dataclass
class ServerState:
    config: MockConfig
    started_unix_ms: int = field(default_factory=lambda: int(time.time() * 1000))
    active_requests: int = 0
    total_requests: dict[tuple[str, str, int], int] = field(default_factory=dict)
    lock: threading.Lock = field(default_factory=threading.Lock)

    def record_start(self) -> None:
        with self.lock:
            self.active_requests += 1

    def record_finish(self, path: str, mode: str, status: int) -> None:
        with self.lock:
            self.active_requests = max(0, self.active_requests - 1)
            key = (path, mode, status)
            self.total_requests[key] = self.total_requests.get(key, 0) + 1

    def metrics_text(self) -> str:
        with self.lock:
            active = self.active_requests
            totals = dict(self.total_requests)
        lines = [
            "# TYPE astra_mock_openai_active_requests gauge",
            f"astra_mock_openai_active_requests {active}",
            "# TYPE astra_mock_openai_requests_total counter",
        ]
        for (path, mode, status), count in sorted(totals.items()):
            lines.append(
                "astra_mock_openai_requests_total"
                f'{{path="{escape_label(path)}",mode="{escape_label(mode)}",status="{status}"}} {count}'
            )
        lines.extend(
            [
                "# TYPE astra_mock_openai_delay_ms gauge",
                f"astra_mock_openai_delay_ms {self.config.delay_ms}",
            ]
        )
        return "\n".join(lines) + "\n"


def escape_label(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def build_completion_payload(config: MockConfig, request: dict[str, Any]) -> dict[str, Any]:
    model = str(request.get("model") or config.model_name)
    prompt_tokens = max(0, config.prompt_tokens)
    completion_tokens = max(0, config.completion_tokens)
    return {
        "id": f"chatcmpl-mock-{int(time.time() * 1000)}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": config.response_content,
                },
                "finish_reason": "stop",
            }
        ],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    }


def build_stream_payloads(config: MockConfig, request: dict[str, Any]) -> list[dict[str, Any]]:
    model = str(request.get("model") or config.model_name)
    created = int(time.time())
    prompt_tokens = max(0, config.prompt_tokens)
    completion_tokens = max(0, config.completion_tokens)
    return [
        {
            "id": f"chatcmpl-mock-{created}",
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {"role": "assistant"},
                    "finish_reason": None,
                }
            ],
        },
        {
            "id": f"chatcmpl-mock-{created}",
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {"content": config.response_content},
                    "finish_reason": None,
                }
            ],
        },
        {
            "id": f"chatcmpl-mock-{created}",
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }
            ],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        },
    ]


def build_error_payload(config: MockConfig, status: int) -> dict[str, Any]:
    return {
        "error": {
            "message": config.error_message,
            "type": "mock_openai_error",
            "code": config.error_code,
            "status": status,
        }
    }


def model_yaml(config: MockConfig, base_url: str) -> str:
    return (
        f"- name: {config.model_name}\n"
        "  provider: openai\n"
        f"  api_key: {config.api_key}\n"
        f"  base_url: {base_url}\n"
        '  description: "Local mock model for capacity probes"\n'
        "  tags: [chat, code, selector]\n"
        "  context_window: 200000\n"
        "  max_completion_tokens: 1024\n"
        "  supported_parameters: [tools]\n"
    )


def make_handler(state: ServerState) -> type[BaseHTTPRequestHandler]:
    class MockOpenAiHandler(BaseHTTPRequestHandler):
        server_version = "astra-mock-openai/1"

        def do_GET(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path == "/health":
                write_json(
                    self,
                    HTTPStatus.OK,
                    {
                        "ok": True,
                        "model": state.config.model_name,
                        "started_unix_ms": state.started_unix_ms,
                    },
                )
                return
            if path == "/models":
                write_json(
                    self,
                    HTTPStatus.OK,
                    {"data": [{"id": state.config.model_name, "object": "model"}]},
                )
                return
            if path == "/metrics":
                write_text(self, HTTPStatus.OK, state.metrics_text(), "text/plain; version=0.0.4")
                return
            write_json(self, HTTPStatus.NOT_FOUND, {"error": {"message": "not found", "code": "not_found"}})

        def do_POST(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path not in COMPLETIONS_PATHS:
                write_json(
                    self,
                    HTTPStatus.NOT_FOUND,
                    {"error": {"message": "not found", "code": "not_found"}},
                )
                return

            state.record_start()
            mode = "unknown"
            status = state.config.status
            try:
                request = read_json_body(self)
                mode = "stream" if bool(request.get("stream")) else "json"
                if state.config.delay_ms > 0:
                    time.sleep(state.config.delay_ms / 1000.0)
                if status >= 400:
                    write_json(self, HTTPStatus(status), build_error_payload(state.config, status))
                    return
                if mode == "stream":
                    write_openai_stream(self, state.config, request)
                else:
                    write_json(self, HTTPStatus.OK, build_completion_payload(state.config, request))
            except ValueError as exc:
                status = HTTPStatus.BAD_REQUEST
                write_json(
                    self,
                    HTTPStatus.BAD_REQUEST,
                    {"error": {"message": str(exc), "code": "bad_request"}},
                )
            finally:
                state.record_finish(path, mode, int(status))

        def log_message(self, fmt: str, *args: Any) -> None:
            return

    return MockOpenAiHandler


def read_json_body(handler: BaseHTTPRequestHandler) -> dict[str, Any]:
    raw_length = handler.headers.get("content-length", "0")
    try:
        length = int(raw_length)
    except ValueError as exc:
        raise ValueError("invalid content-length") from exc
    if length > 16 * 1024 * 1024:
        raise ValueError("request body exceeds 16 MiB")
    body = handler.rfile.read(length) if length > 0 else b"{}"
    try:
        parsed = json.loads(body.decode("utf-8"))
    except json.JSONDecodeError as exc:
        raise ValueError("request body is not valid JSON") from exc
    if not isinstance(parsed, dict):
        raise ValueError("request body must be a JSON object")
    return parsed


def write_openai_stream(
    handler: BaseHTTPRequestHandler,
    config: MockConfig,
    request: dict[str, Any],
) -> None:
    handler.send_response(HTTPStatus.OK)
    handler.send_header("content-type", "text/event-stream")
    handler.send_header("cache-control", "no-cache")
    handler.send_header("connection", "close")
    handler.end_headers()
    for payload in build_stream_payloads(config, request):
        handler.wfile.write(b"data: ")
        handler.wfile.write(json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8"))
        handler.wfile.write(b"\n\n")
        handler.wfile.flush()
    handler.wfile.write(b"data: [DONE]\n\n")
    handler.wfile.flush()


def write_json(handler: BaseHTTPRequestHandler, status: HTTPStatus, payload: dict[str, Any]) -> None:
    body = json.dumps(payload, separators=(",", ":"), sort_keys=True).encode("utf-8")
    handler.send_response(status)
    handler.send_header("content-type", "application/json")
    handler.send_header("content-length", str(len(body)))
    handler.send_header("connection", "close")
    handler.end_headers()
    handler.wfile.write(body)


def write_text(handler: BaseHTTPRequestHandler, status: HTTPStatus, text: str, content_type: str) -> None:
    body = text.encode("utf-8")
    handler.send_response(status)
    handler.send_header("content-type", content_type)
    handler.send_header("content-length", str(len(body)))
    handler.send_header("connection", "close")
    handler.end_headers()
    handler.wfile.write(body)


def raise_nofile_limit(
    target: int,
    *,
    quiet: bool,
    resource_module: Any | None = None,
) -> tuple[int | None, int | None]:
    if target <= 0:
        return None, None
    try:
        if resource_module is None:
            import resource as resource_module  # type: ignore[no-redef]

        soft, hard = resource_module.getrlimit(resource_module.RLIMIT_NOFILE)
        desired = max(soft, target)
        if hard != resource_module.RLIM_INFINITY:
            desired = min(desired, hard)
        if desired > soft:
            resource_module.setrlimit(resource_module.RLIMIT_NOFILE, (desired, hard))
            soft = desired
        return int(soft), int(hard)
    except Exception as exc:  # noqa: BLE001
        if not quiet:
            print(f"warning: failed to raise nofile limit: {exc}", file=sys.stderr)
        return None, None


def serve(args: argparse.Namespace) -> int:
    nofile_soft, nofile_hard = raise_nofile_limit(args.nofile_target, quiet=args.quiet)
    config = MockConfig(
        model_name=args.model_name,
        response_content=args.response_content,
        prompt_tokens=args.prompt_tokens,
        completion_tokens=args.completion_tokens,
        delay_ms=args.delay_ms,
        status=args.status,
        error_code=args.error_code,
        error_message=args.error_message,
        api_key=args.api_key,
    )
    state = ServerState(config)
    server = CapacityMockOpenAiServer((args.host, args.port), make_handler(state))
    host, port = server.server_address
    base_url = f"http://{host}:{port}"
    if args.write_model_yaml:
        path = Path(args.write_model_yaml)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(model_yaml(config, base_url), encoding="utf-8")
    if not args.quiet:
        print(
            json.dumps(
                {
                    "base_url": base_url,
                    "chat_completions_url": f"{base_url}/chat/completions",
                    "model_name": config.model_name,
                    "model_yaml": args.write_model_yaml,
                    "nofile_soft": nofile_soft,
                    "nofile_hard": nofile_hard,
                    "status": config.status,
                },
                indent=2,
                sort_keys=True,
            ),
            flush=True,
        )

    stop = threading.Event()

    def request_shutdown(_signum: int, _frame: Any) -> None:
        stop.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    previous_int = signal.signal(signal.SIGINT, request_shutdown)
    previous_term = signal.signal(signal.SIGTERM, request_shutdown)
    try:
        server.serve_forever(poll_interval=0.2)
    finally:
        signal.signal(signal.SIGINT, previous_int)
        signal.signal(signal.SIGTERM, previous_term)
        server.server_close()
        stop.set()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18080)
    parser.add_argument("--model-name", default="capacity-mock")
    parser.add_argument("--api-key", default="mock-key")
    parser.add_argument("--response-content", default="mock capacity response")
    parser.add_argument("--prompt-tokens", type=int, default=128)
    parser.add_argument("--completion-tokens", type=int, default=16)
    parser.add_argument("--delay-ms", type=int, default=0)
    parser.add_argument("--status", type=int, default=200)
    parser.add_argument("--error-code", default="mock_openai_error")
    parser.add_argument("--error-message", default="mock OpenAI-compatible error")
    parser.add_argument("--write-model-yaml")
    parser.add_argument(
        "--nofile-target",
        type=int,
        default=DEFAULT_NOFILE_TARGET,
        help="raise soft file-descriptor limit to at least this value when possible; use 0 to disable",
    )
    parser.add_argument("--quiet", action="store_true")
    return parser


def validate_args(args: argparse.Namespace) -> None:
    if args.port < 0 or args.port > 65535:
        raise ValueError("--port must be between 0 and 65535")
    if args.prompt_tokens < 0 or args.completion_tokens < 0:
        raise ValueError("token counts must be non-negative")
    if args.delay_ms < 0:
        raise ValueError("--delay-ms must be non-negative")
    if args.status < 100 or args.status > 599:
        raise ValueError("--status must be a valid HTTP status")
    if not args.model_name.strip():
        raise ValueError("--model-name must be non-empty")
    if args.nofile_target < 0:
        raise ValueError("--nofile-target must be non-negative")


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        validate_args(args)
        return serve(args)
    except KeyboardInterrupt:
        return 130
    except Exception as exc:  # noqa: BLE001
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
