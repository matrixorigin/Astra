#!/usr/bin/env python3
"""Unit tests for durable_work_pressure_probe.py."""

from __future__ import annotations

import argparse
import asyncio
import importlib.util
import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace


SCRIPT = Path(__file__).with_name("durable_work_pressure_probe.py")
sys.path.insert(0, str(SCRIPT.parent))
SPEC = importlib.util.spec_from_file_location("durable_work_pressure_probe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)


class DurableWorkPressureProbeTests(unittest.TestCase):
    def test_config_defaults_are_bounded_and_owner_shaped(self) -> None:
        args = probe.build_parser().parse_args(["--profile", "smoke", "--dry-run"])
        config = probe.config_from_args(args)
        self.assertEqual(config.owners, 3)
        self.assertEqual(config.sessions_per_owner, 2)
        self.assertEqual(config.total_sessions, 6)
        self.assertEqual(config.read_rate, 2.0)
        self.assertEqual(config.write_rate, 1.0)

    def test_dry_run_reports_dataset_without_network_access(self) -> None:
        output = io.StringIO()
        with redirect_stdout(output):
            self.assertEqual(
                probe.main(
                    [
                        "--profile",
                        "smoke",
                        "--owners",
                        "2",
                        "--sessions-per-owner",
                        "3",
                        "--dry-run",
                    ]
                ),
                0,
            )
        value = json.loads(output.getvalue())
        self.assertEqual(value["dataset_works"], 6)
        self.assertEqual(value["dataset_read_attachments"], 12)
        self.assertEqual(value["slow_session_index"], 0)

    def test_config_rejects_missing_slow_session(self) -> None:
        args = probe.build_parser().parse_args(
            ["--owners", "2", "--sessions-per-owner", "2", "--slow-session-index", "4"]
        )
        with self.assertRaises(probe.ProbeError):
            probe.config_from_args(args)

    def test_config_requires_reader_and_writer_coverage(self) -> None:
        args = probe.build_parser().parse_args(
            ["--owners", "2", "--sessions-per-owner", "1"]
        )
        with self.assertRaises(probe.ProbeError):
            probe.config_from_args(args)
        args = probe.build_parser().parse_args(
            ["--owners", "2", "--sessions-per-owner", "2", "--duration-secs", "0.1"]
        )
        with self.assertRaises(probe.ProbeError):
            probe.config_from_args(args)
        args = probe.build_parser().parse_args(
            ["--owners", "2", "--sessions-per-owner", "2", "--duration-secs", "nan"]
        )
        with self.assertRaises(probe.ProbeError):
            probe.config_from_args(args)

    def test_parse_tokens_deduplicates_bearer_values(self) -> None:
        from tempfile import TemporaryDirectory

        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "tokens.json"
            path.write_text(
                json.dumps({"tokens": ["Bearer first", "second", "Bearer first"]}),
                encoding="utf-8",
            )
            self.assertEqual(
                probe.parse_tokens(str(path), "Bearer second"),
                ["second", "first"],
            )

    def test_read_paths_are_canonical_and_bounded(self) -> None:
        target = probe.WorkTarget(1, 2, "token", "work-1", "branch-1", ("a", "b"), (1, 2))
        self.assertEqual(probe.read_path(target, "catalog"), "/v1/works?limit=16")
        self.assertIn("item_limit=16", probe.read_path(target, "task_graph"))
        self.assertTrue(probe.read_path(target, "transcript").endswith("transcript?limit=16"))

    def test_operation_summary_keeps_latency_and_failure_evidence(self) -> None:
        values = [
            probe.OperationResult("events", 0, 0, 200, 10.0, 30, "ok"),
            probe.OperationResult("events", 0, 0, 503, 20.0, 50, "http_error", "busy"),
        ]
        summary = probe.summarize_operation_results(values)
        self.assertEqual(summary["events"]["samples"], 2)
        self.assertEqual(summary["events"]["outcomes"], {"http_error": 1, "ok": 1})
        self.assertEqual(summary["events"]["status"], {"200": 1, "503": 1})
        self.assertEqual(summary["events"]["errors"][0]["error_code"], "busy")

    def test_metrics_summary_explicitly_reports_missing_families(self) -> None:
        summary = probe.summarize_metrics(
            [
                {
                    "kind": "final",
                    "http_status": 200,
                    "metrics": {"astra_run_admission_attempts_total{outcome=\"ok\"}": 2},
                }
            ]
        )
        self.assertEqual(summary["samples_with_metrics"], 1)
        self.assertEqual(summary["successful_samples_with_metrics"], 1)
        self.assertIn("astra_durable_run_event_", summary["missing_expected_families"])

        unavailable = probe.summarize_metrics(
            [{"kind": "final", "http_status": 503, "metrics": {"astra_run_admission_attempts_total": 1}}]
        )
        self.assertEqual(unavailable["samples_with_metrics"], 1)
        self.assertEqual(unavailable["successful_samples_with_metrics"], 0)

    def test_foreign_rejection_requires_typed_code_and_hides_identity(self) -> None:
        source = probe.WorkTarget(0, 0, "source-token", "source-work", "source-branch", ("a", "b"), (1, 2))
        foreign = probe.WorkTarget(1, 0, "foreign-token", "foreign-work", "foreign-branch", ("c", "d"), (1, 2))
        original = probe.request_json

        async def fake_request_json(*_args, **_kwargs):
            return (
                probe.OperationResult("foreign", 1, 0, 404, 1.0, 40, "http_error", "wrong_code"),
                {"code": "wrong_code", "work_id": "source-work"},
            )

        probe.request_json = fake_request_json
        try:
            results = asyncio.run(probe.check_foreign_access(object(), source, foreign))
        finally:
            probe.request_json = original
        self.assertTrue(all(result.outcome == "foreign_identity_leak" for result in results))

    def test_request_json_retains_bounded_error_json_and_accepts_empty_delete(self) -> None:
        config = SimpleNamespace(
            base_url="http://example.test",
            connect_timeout_secs=1.0,
            request_timeout_secs=1.0,
        )
        original = probe.http_request

        async def error_response(*_args, **_kwargs):
            return probe.HttpResponse(
                404,
                "Not Found",
                {},
                b'{"code":"wrong_code","work_id":"source-work"}',
                1.0,
            )

        probe.http_request = error_response
        try:
            result, value = asyncio.run(
                probe.request_json(
                    config,
                    "foreign",
                    1,
                    0,
                    "token",
                    "GET",
                    "/v1/works/source-work",
                )
            )
        finally:
            probe.http_request = original
        self.assertEqual(result.outcome, "http_error")
        self.assertEqual(value, {"code": "wrong_code", "work_id": "source-work"})

        async def empty_response(*_args, **_kwargs):
            return probe.HttpResponse(204, "No Content", {}, b"", 1.0)

        probe.http_request = empty_response
        try:
            result, value = asyncio.run(
                probe.request_json(
                    config,
                    "detach",
                    1,
                    0,
                    "token",
                    "DELETE",
                    "/v1/works/work/branches/branch/attachments/attachment",
                    allow_empty=True,
                )
            )
        finally:
            probe.http_request = original
        self.assertEqual(result.outcome, "ok")
        self.assertIsNone(value)

    def test_pressure_write_requires_epoch_to_advance_past_initial_attachments(self) -> None:
        target = probe.WorkTarget(
            0,
            0,
            "token",
            "work",
            "branch",
            ("initial-a", "initial-b"),
            (1, 2),
        )
        config = object()
        original = probe.request_json

        async def fake_request_json(_config, operation, *_args, **_kwargs):
            if operation == "write_attachment_open":
                return (
                    probe.OperationResult(operation, 0, 0, 200, 1.0, 20, "ok"),
                    {
                        "work_id": "work",
                        "branch_id": "branch",
                        "attachment_id": "new-attachment",
                        "attachment_epoch": 2,
                        "mode": "read_only",
                    },
                )
            if operation == "write_attachment_close":
                return probe.OperationResult(operation, 0, 0, 204, 1.0, 0, "ok"), None
            return probe.OperationResult(operation, 0, 0, 200, 1.0, 20, "ok"), {"through_event_seq": 1}

        probe.request_json = fake_request_json
        try:
            results, _ = asyncio.run(probe.run_write(config, target, 0, 2))
        finally:
            probe.request_json = original
        self.assertEqual(results[0].outcome, "epoch_not_monotonic")


if __name__ == "__main__":
    unittest.main()
