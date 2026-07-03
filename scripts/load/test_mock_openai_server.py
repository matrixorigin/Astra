#!/usr/bin/env python3
"""Unit tests for mock_openai_server.py."""

from __future__ import annotations

import http.client
import importlib.util
import json
import sys
import unittest
import threading
from pathlib import Path
from typing import Any


SCRIPT = Path(__file__).with_name("mock_openai_server.py")
SPEC = importlib.util.spec_from_file_location("mock_openai_server", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
mock = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = mock
SPEC.loader.exec_module(mock)


class RunningServer:
    def __init__(self, config: Any) -> None:
        self.state = mock.ServerState(config)
        self.server = mock.CapacityMockOpenAiServer(("127.0.0.1", 0), mock.make_handler(self.state))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.host, self.port = self.server.server_address

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> tuple[int, str]:
        payload = json.dumps(body or {}, separators=(",", ":")).encode("utf-8")
        conn = http.client.HTTPConnection(self.host, self.port, timeout=5)
        try:
            conn.request(
                method,
                path,
                body=payload if method == "POST" else None,
                headers={"content-type": "application/json"} if method == "POST" else {},
            )
            response = conn.getresponse()
            data = response.read().decode("utf-8")
            return response.status, data
        finally:
            conn.close()


class MockOpenAiServerTests(unittest.TestCase):
    def config(self, **overrides: Any) -> Any:
        values = {
            "model_name": "capacity-mock",
            "response_content": "mock capacity response",
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "delay_ms": 0,
            "status": 200,
            "error_code": "mock_openai_error",
            "error_message": "mock error",
            "api_key": "mock-key",
        }
        values.update(overrides)
        return mock.MockConfig(**values)

    def test_non_streaming_chat_completions_response_is_openai_compatible(self) -> None:
        server = RunningServer(self.config())
        try:
            status, body = server.request(
                "POST",
                "/chat/completions",
                {"model": "capacity-mock", "messages": [{"role": "user", "content": "hi"}]},
            )
            parsed = json.loads(body)
            self.assertEqual(status, 200)
            self.assertEqual(parsed["choices"][0]["message"]["role"], "assistant")
            self.assertEqual(parsed["choices"][0]["message"]["content"], "mock capacity response")
            self.assertEqual(parsed["choices"][0]["finish_reason"], "stop")
            self.assertEqual(parsed["usage"]["prompt_tokens"], 11)
            self.assertEqual(parsed["usage"]["completion_tokens"], 7)

            metrics_status, metrics = server.request("GET", "/metrics")
            self.assertEqual(metrics_status, 200)
            self.assertIn('astra_mock_openai_requests_total{path="/chat/completions",mode="json",status="200"} 1', metrics)
        finally:
            server.close()

    def test_streaming_v1_chat_completions_response_is_sse(self) -> None:
        server = RunningServer(self.config(response_content="stream response"))
        try:
            status, body = server.request(
                "POST",
                "/v1/chat/completions",
                {
                    "model": "capacity-mock",
                    "stream": True,
                    "messages": [{"role": "user", "content": "hi"}],
                },
            )
            self.assertEqual(status, 200)
            self.assertIn("data: ", body)
            self.assertIn('"object":"chat.completion.chunk"', body)
            self.assertIn('"content":"stream response"', body)
            self.assertIn('"usage":', body)
            self.assertTrue(body.endswith("data: [DONE]\n\n"))

            metrics_status, metrics = server.request("GET", "/metrics")
            self.assertEqual(metrics_status, 200)
            self.assertIn('astra_mock_openai_requests_total{path="/v1/chat/completions",mode="stream",status="200"} 1', metrics)
        finally:
            server.close()

    def test_error_status_returns_openai_error_json(self) -> None:
        server = RunningServer(
            self.config(status=429, error_code="rate_limit_exceeded", error_message="slow down")
        )
        try:
            status, body = server.request(
                "POST",
                "/chat/completions",
                {"model": "capacity-mock", "stream": True},
            )
            parsed = json.loads(body)
            self.assertEqual(status, 429)
            self.assertEqual(parsed["error"]["code"], "rate_limit_exceeded")
            self.assertEqual(parsed["error"]["message"], "slow down")

            metrics_status, metrics = server.request("GET", "/metrics")
            self.assertEqual(metrics_status, 200)
            self.assertIn('status="429"', metrics)
        finally:
            server.close()

    def test_model_yaml_points_astra_model_loader_at_mock_base_url(self) -> None:
        text = mock.model_yaml(self.config(), "http://127.0.0.1:18080")
        self.assertIn("- name: capacity-mock", text)
        self.assertIn("provider: openai", text)
        self.assertIn("api_key: mock-key", text)
        self.assertIn("base_url: http://127.0.0.1:18080", text)
        self.assertIn("supported_parameters: [tools]", text)

    def test_server_backlog_is_large_enough_for_capacity_probes(self) -> None:
        self.assertGreaterEqual(mock.CapacityMockOpenAiServer.request_queue_size, 512)
        self.assertTrue(mock.CapacityMockOpenAiServer.daemon_threads)

    def test_default_nofile_target_matches_capacity_probe_needs(self) -> None:
        args = mock.build_parser().parse_args([])
        self.assertGreaterEqual(args.nofile_target, 4096)

    def test_raise_nofile_limit_raises_soft_limit_when_possible(self) -> None:
        class FakeResource:
            RLIMIT_NOFILE = object()
            RLIM_INFINITY = -1

            def __init__(self) -> None:
                self.limit = (256, 8192)

            def getrlimit(self, _name: object) -> tuple[int, int]:
                return self.limit

            def setrlimit(self, _name: object, limit: tuple[int, int]) -> None:
                self.limit = limit

        fake = FakeResource()
        soft, hard = mock.raise_nofile_limit(4096, quiet=True, resource_module=fake)
        self.assertEqual((soft, hard), (4096, 8192))
        self.assertEqual(fake.limit, (4096, 8192))


if __name__ == "__main__":
    unittest.main()
