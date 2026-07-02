#!/usr/bin/env python3
"""Unit tests for multi_cli_capacity_probe.py."""

from __future__ import annotations

import argparse
import asyncio
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("multi_cli_capacity_probe.py")
SPEC = importlib.util.spec_from_file_location("multi_cli_capacity_probe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)


class CapacityProbeTests(unittest.TestCase):
    def test_parse_url_preserves_full_path_and_query(self) -> None:
        parsed = probe.parse_url("http://127.0.0.1:17001/metrics?x=1", None)
        self.assertEqual(parsed.target, "/metrics?x=1")
        self.assertEqual(parsed.authority, "127.0.0.1:17001")

    def test_parse_url_can_override_path(self) -> None:
        parsed = probe.parse_url("https://api.example.test/root?old=1", "/chat/stream")
        self.assertEqual(parsed.target, "/chat/stream?old=1")
        self.assertEqual(parsed.authority, "api.example.test")
        self.assertTrue(parsed.use_tls)

    def test_sse_parser_handles_split_frames(self) -> None:
        parser = probe.SseParser()
        self.assertEqual(parser.feed('data: {"type":"session_info"'), [])
        events = parser.feed(',"run_id":"r1"}\n\n')
        self.assertEqual(events, [{"type": "session_info", "run_id": "r1"}])

    def test_sse_parser_reports_malformed_json(self) -> None:
        events = probe.parse_sse_events_from_text("data: not-json\n\n")
        self.assertEqual(events[0]["type"], "malformed")
        self.assertEqual(events[0]["raw"], "not-json")

    def test_prometheus_parser_filters_capacity_metrics(self) -> None:
        metrics = probe.parse_prometheus_metrics(
            "\n".join(
                [
                    "# TYPE astra_capacity_run_slots_total gauge",
                    "astra_capacity_run_slots_total 100",
                    'astra_run_admission_attempts_total{outcome="timeout"} 2',
                    'astra_turn_observer_dispatches_total{mode="async",outcome="scheduled"} 3',
                    'astra_post_loop_memory_cleanup_dispatches_total{mode="async",outcome="dropped_full"} 4',
                    'astra_session_memory_post_loop_drains_total{outcome="leftover"} 5',
                    "process_cpu_seconds_total 9",
                ]
            )
        )
        self.assertEqual(metrics["astra_capacity_run_slots_total"], 100.0)
        self.assertEqual(
            metrics['astra_run_admission_attempts_total{outcome="timeout"}'],
            2.0,
        )
        self.assertEqual(
            metrics['astra_turn_observer_dispatches_total{mode="async",outcome="scheduled"}'],
            3.0,
        )
        self.assertEqual(
            metrics[
                'astra_post_loop_memory_cleanup_dispatches_total{mode="async",outcome="dropped_full"}'
            ],
            4.0,
        )
        self.assertEqual(
            metrics['astra_session_memory_post_loop_drains_total{outcome="leftover"}'],
            5.0,
        )
        self.assertNotIn("process_cpu_seconds_total", metrics)

    def test_default_body_omits_session_id_unless_explicit(self) -> None:
        args = argparse.Namespace(
            message="hello {request_id} {user_index} {profile}",
            profile="100-cli",
            agent_id=None,
            model="gpt-test",
            session_id_template=None,
        )
        body = probe.default_body(args, request_id=7, user_index=3)
        self.assertEqual(body["message"], "hello 7 3 100-cli")
        self.assertNotIn("session_id", body)
        self.assertEqual(body["selected_model"], {"model": "gpt-test"})

    def test_body_template_renders_placeholders(self) -> None:
        args = argparse.Namespace(
            message="msg {request_id}",
            profile="500-cli",
            model=None,
        )
        template = {
            "message": "{message}",
            "context": {"rid": "{request_id}", "user": "{user_index}"},
        }
        rendered = json.loads(probe.body_for_request(args, template, 9, 4).decode("utf-8"))
        self.assertEqual(rendered["message"], "msg 9")
        self.assertEqual(rendered["context"], {"rid": "9", "user": "4"})

    def test_stream_body_contract_requires_selected_model(self) -> None:
        args = argparse.Namespace(
            message="msg {request_id}",
            profile="100-cli",
            agent_id=None,
            model=None,
            session_id_template=None,
        )
        with self.assertRaises(probe.ProbeError):
            probe.validate_stream_body_contract(args, None)

        template = {
            "message": "{message}",
            "selected_model": {"model": "gpt-test"},
        }
        probe.validate_stream_body_contract(args, template)

    def test_summarize_metrics_file_reports_empty_prefixed_samples(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"unix_ms": 10, "http_status": 200, "metrics": {}}),
                        json.dumps(
                            {
                                "unix_ms": 20,
                                "http_status": 200,
                                "metrics": {"astra_capacity_run_slots_total": 100.0},
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        self.assertEqual(summary["sample_count"], 2)
        self.assertEqual(summary["samples_with_metrics"], 1)
        self.assertEqual(summary["http_status"], {"200": 2})
        self.assertEqual(summary["last_metric_count"], 1)
        self.assertEqual(summary["last_metric_names"], ["astra_capacity_run_slots_total"])

    def test_error_helpers_read_run_lifecycle_machine_code(self) -> None:
        event = {
            "type": "run_error",
            "error": "[network] LLM request failed",
            "error_code": "network",
            "code": "LLM_TRANSPORT_ERROR",
        }
        self.assertEqual(probe.error_code_from_event(event), "network")
        self.assertEqual(probe.error_message_from_event(event), "[network] LLM request failed")

        legacy_event = {"type": "error", "message": "boom", "code": "INTERNAL_ERROR"}
        self.assertEqual(probe.error_code_from_event(legacy_event), "INTERNAL_ERROR")
        self.assertEqual(probe.error_message_from_event(legacy_event), "boom")

    def test_http_error_body_helpers_read_machine_code(self) -> None:
        code, message, retryable = probe.error_details_from_http_body(
            json.dumps(
                {
                    "error_code": "run_admission_timeout",
                    "detail": "server capacity timeout",
                    "retryable": True,
                }
            )
        )
        self.assertEqual(code, "run_admission_timeout")
        self.assertEqual(message, "server capacity timeout")
        self.assertTrue(retryable)

        nested_code, nested_message, nested_retryable = probe.error_details_from_http_body(
            json.dumps(
                {
                    "error": {
                        "code": "per_user_concurrent_session_quota",
                        "message": "quota exceeded",
                        "retryable": False,
                    }
                }
            )
        )
        self.assertEqual(nested_code, "per_user_concurrent_session_quota")
        self.assertEqual(nested_message, "quota exceeded")
        self.assertFalse(nested_retryable)

        text_code, text_message, text_retryable = probe.error_details_from_http_body("plain boom")
        self.assertIsNone(text_code)
        self.assertEqual(text_message, "plain boom")
        self.assertIsNone(text_retryable)

    def test_stream_request_reads_http_error_json_body(self) -> None:
        async def run() -> None:
            async def handle(
                reader: asyncio.StreamReader,
                writer: asyncio.StreamWriter,
            ) -> None:
                await reader.readuntil(b"\r\n\r\n")
                body = json.dumps(
                    {
                        "error_code": "run_admission_timeout",
                        "detail": "server capacity timeout",
                        "retryable": True,
                    }
                ).encode("utf-8")
                writer.write(
                    b"HTTP/1.1 503 Service Unavailable\r\n"
                    b"content-type: application/json\r\n"
                    + f"content-length: {len(body)}\r\n".encode("ascii")
                    + b"connection: close\r\n\r\n"
                    + body
                )
                await writer.drain()
                writer.close()
                await writer.wait_closed()

            server = await asyncio.start_server(handle, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]
                result = await probe.stream_sse_request(
                    request_id=1,
                    user_index=1,
                    token_index=None,
                    url_text=f"http://127.0.0.1:{port}/chat/stream",
                    headers={},
                    body=b"{}",
                    connect_timeout_secs=1,
                    request_timeout_secs=1,
                )
            finally:
                server.close()
                await server.wait_closed()

            self.assertEqual(result.outcome, "http_error")
            self.assertEqual(result.http_status, 503)
            self.assertEqual(result.error_code, "run_admission_timeout")
            self.assertEqual(result.error_message, "server capacity timeout")
            self.assertTrue(result.retryable)

        asyncio.run(run())

    def test_summarize_results_reports_terminal_failure_reasons(self) -> None:
        args = argparse.Namespace(
            profile="100-cli",
            base_url="http://127.0.0.1:17001",
            endpoint="/chat/stream",
            concurrency=2,
            total=3,
            output_dir=Path("tmp/probe-test"),
        )
        results = [
            probe.StreamResult(
                request_id=0,
                user_index=0,
                token_index=0,
                http_status=200,
                header_latency_ms=1.0,
                first_event_ms=1.0,
                duration_ms=10.0,
                event_count=3,
                session_id="s0",
                run_id="r0",
                terminal_status="completed",
                error_code=None,
                error_message=None,
                retryable=None,
                outcome="completed",
            ),
            probe.StreamResult(
                request_id=1,
                user_index=1,
                token_index=1,
                http_status=200,
                header_latency_ms=2.0,
                first_event_ms=2.0,
                duration_ms=20.0,
                event_count=3,
                session_id="s1",
                run_id="r1",
                terminal_status="failed",
                error_code=None,
                error_message=None,
                retryable=None,
                outcome="terminal_non_success",
            ),
            probe.StreamResult(
                request_id=2,
                user_index=2,
                token_index=2,
                http_status=503,
                header_latency_ms=2.0,
                first_event_ms=None,
                duration_ms=5.0,
                event_count=0,
                session_id=None,
                run_id=None,
                terminal_status=None,
                error_code="run_admission_timeout",
                error_message="server capacity timeout",
                retryable=True,
                outcome="http_error",
            ),
        ]

        summary = probe.summarize_results(
            results,
            args,
            started_unix_ms=1,
            ended_unix_ms=2,
            elapsed_ms=30.0,
            metrics_summary={"sample_count": 0, "samples_with_metrics": 0},
            contract_violations=[],
        )

        self.assertEqual(summary["terminal_statuses"], {"completed": 1, "failed": 1, "none": 1})
        self.assertEqual(
            summary["failure_reasons"],
            {"error_code:run_admission_timeout": 1, "terminal_status:failed": 1},
        )
        self.assertEqual(summary["error_codes"], {"none": 1, "run_admission_timeout": 1})
        self.assertEqual(summary["failures_missing_error_code"], 1)

    def test_output_writer_keeps_jsonl_handles_open_until_closed(self) -> None:
        async def run(path: Path) -> probe.OutputWriter:
            writer = probe.OutputWriter(path)
            await writer.write_request(
                probe.StreamResult(
                    request_id=0,
                    user_index=0,
                    token_index=0,
                    http_status=200,
                    header_latency_ms=1.0,
                    first_event_ms=1.0,
                    duration_ms=2.0,
                    event_count=1,
                    session_id="s",
                    run_id="r",
                    terminal_status="completed",
                    error_code=None,
                    error_message=None,
                    retryable=None,
                    outcome="completed",
                )
            )
            await writer.write_metrics(
                {"unix_ms": 1, "http_status": 200, "metrics": {"astra_capacity_run_slots_total": 50}},
                "astra_capacity_run_slots_total 50\n",
            )
            return writer

        with tempfile.TemporaryDirectory() as tmp:
            writer = asyncio.run(run(Path(tmp)))
            self.assertFalse(writer._requests_file.closed)
            self.assertFalse(writer._metrics_file.closed)
            self.assertFalse(writer._metrics_raw_file.closed)
            writer.close()
            self.assertTrue(writer._requests_file.closed)
            self.assertTrue(writer._metrics_file.closed)
            self.assertTrue(writer._metrics_raw_file.closed)
            self.assertEqual(len((Path(tmp) / "requests.jsonl").read_text(encoding="utf-8").splitlines()), 1)
            self.assertIn(
                "astra_capacity_run_slots_total",
                (Path(tmp) / "metrics.jsonl").read_text(encoding="utf-8"),
            )

    def test_percentile_summary_handles_empty(self) -> None:
        self.assertEqual(
            probe.percentile_summary([]),
            {"min": None, "p50": None, "p95": None, "p99": None, "max": None},
        )


if __name__ == "__main__":
    unittest.main()
