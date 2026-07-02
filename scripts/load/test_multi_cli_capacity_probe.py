#!/usr/bin/env python3
"""Unit tests for multi_cli_capacity_probe.py."""

from __future__ import annotations

import argparse
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

    def test_summarize_results_reports_terminal_failure_reasons(self) -> None:
        args = argparse.Namespace(
            profile="100-cli",
            base_url="http://127.0.0.1:17001",
            endpoint="/chat/stream",
            concurrency=2,
            total=2,
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

        self.assertEqual(summary["terminal_statuses"], {"completed": 1, "failed": 1})
        self.assertEqual(summary["failure_reasons"], {"terminal_status:failed": 1})

    def test_percentile_summary_handles_empty(self) -> None:
        self.assertEqual(
            probe.percentile_summary([]),
            {"min": None, "p50": None, "p95": None, "p99": None, "max": None},
        )


if __name__ == "__main__":
    unittest.main()
