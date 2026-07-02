#!/usr/bin/env python3
"""Unit tests for multi_cli_capacity_probe.py."""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
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
                    "process_cpu_seconds_total 9",
                ]
            )
        )
        self.assertEqual(metrics["astra_capacity_run_slots_total"], 100.0)
        self.assertEqual(
            metrics['astra_run_admission_attempts_total{outcome="timeout"}'],
            2.0,
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
        )
        template = {
            "message": "{message}",
            "context": {"rid": "{request_id}", "user": "{user_index}"},
        }
        rendered = json.loads(probe.body_for_request(args, template, 9, 4).decode("utf-8"))
        self.assertEqual(rendered["message"], "msg 9")
        self.assertEqual(rendered["context"], {"rid": "9", "user": "4"})

    def test_percentile_summary_handles_empty(self) -> None:
        self.assertEqual(
            probe.percentile_summary([]),
            {"min": None, "p50": None, "p95": None, "p99": None, "max": None},
        )


if __name__ == "__main__":
    unittest.main()
