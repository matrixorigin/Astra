#!/usr/bin/env python3
"""Unit tests for multi_cli_capacity_probe.py."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import io
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
                    'astra_run_control_poll_attempts_total{operation="status",outcome="ok"} 8',
                    'astra_run_recovery_runs_total{action="fail_crashed",outcome="committed"} 1',
                    'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"} 9',
                    "astra_edge_dispatch_claimed_total 10",
                    "astra_event_ingestion_events_dropped_before_acceptance_total 6",
                    'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="critical"} 0',
                    'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="telemetry"} 7',
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
        self.assertEqual(
            metrics['astra_run_control_poll_attempts_total{operation="status",outcome="ok"}'],
            8.0,
        )
        self.assertEqual(
            metrics[
                'astra_run_recovery_runs_total{action="fail_crashed",outcome="committed"}'
            ],
            1.0,
        )
        self.assertEqual(
            metrics[
                'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"}'
            ],
            9.0,
        )
        self.assertEqual(metrics["astra_edge_dispatch_claimed_total"], 10.0)
        self.assertEqual(
            metrics["astra_event_ingestion_events_dropped_before_acceptance_total"],
            6.0,
        )
        self.assertEqual(
            metrics[
                'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="critical"}'
            ],
            0.0,
        )
        self.assertEqual(
            metrics[
                'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="telemetry"}'
            ],
            7.0,
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

    def test_nofile_capacity_preflight_rejects_low_soft_limit(self) -> None:
        original = probe.current_nofile_soft_limit
        try:
            probe.current_nofile_soft_limit = lambda: 256
            with self.assertRaises(probe.ProbeError) as ctx:
                probe.validate_nofile_capacity(500)
            self.assertIn("file descriptor limit too low", str(ctx.exception))
            self.assertIn("ulimit -n 4096", str(ctx.exception))
        finally:
            probe.current_nofile_soft_limit = original

    def test_nofile_capacity_estimate_has_headroom(self) -> None:
        self.assertEqual(probe.estimated_nofile_required(10), 74)
        self.assertEqual(probe.estimated_nofile_required(500), 564)

    def test_validate_args_skips_nofile_failure_for_dry_run_only(self) -> None:
        def args(dry_run: bool) -> argparse.Namespace:
            return argparse.Namespace(
                profile="500-cli",
                concurrency=None,
                total=None,
                register_concurrency=20,
                connect_timeout_secs=10.0,
                request_timeout_secs=300.0,
                metrics_interval_secs=5.0,
                max_control_plane_polls_per_worker_per_sec=8.0,
                max_edge_dispatch_errors_per_sec=None,
                require_no_run_control_errors=False,
                require_no_durable_event_errors=False,
                require_no_edge_dispatch_errors=False,
                skip_nofile_check=False,
                dry_run=dry_run,
            )

        original = probe.current_nofile_soft_limit
        try:
            probe.current_nofile_soft_limit = lambda: 256
            probe.validate_args(args(dry_run=True))
            with self.assertRaises(probe.ProbeError):
                probe.validate_args(args(dry_run=False))
        finally:
            probe.current_nofile_soft_limit = original

    def test_validate_args_allows_disabling_control_plane_poll_contract(self) -> None:
        args = argparse.Namespace(
            profile="100-cli",
            concurrency=10,
            total=10,
            register_concurrency=20,
            connect_timeout_secs=10.0,
            request_timeout_secs=300.0,
            metrics_interval_secs=5.0,
            max_control_plane_polls_per_worker_per_sec=-1.0,
            max_edge_dispatch_errors_per_sec=None,
            require_no_run_control_errors=False,
            require_no_durable_event_errors=False,
            require_no_edge_dispatch_errors=False,
            skip_nofile_check=True,
            dry_run=False,
        )

        probe.validate_args(args)

        self.assertIsNone(args.max_control_plane_polls_per_worker_per_sec)

    def test_run_probe_records_final_metrics_sample_after_workers_finish(self) -> None:
        async def run_case(output_dir: Path) -> int:
            args = argparse.Namespace(
                profile="100-cli",
                concurrency=1,
                total=1,
                output_dir=output_dir,
                body_template=None,
                dry_run=False,
                base_url="http://127.0.0.1:17001",
                endpoint="/chat/stream",
                metrics_path="/metrics",
                metrics_auth_token=None,
                auth_token="test-token",
                token_file=None,
                register_users=False,
                register_concurrency=1,
                require_distinct_users=True,
                user_mode="worker",
                message="capacity probe request {request_id}",
                agent_id=None,
                model="capacity-mock",
                session_id_template=None,
                connect_timeout_secs=1.0,
                request_timeout_secs=5.0,
                metrics_interval_secs=60.0,
                require_metrics=True,
                require_error_codes_for_failures=True,
                require_no_critical_ingestion_drops=False,
                require_no_run_control_errors=False,
                require_no_durable_event_errors=False,
                require_no_edge_dispatch_errors=False,
                max_control_plane_polls_per_worker_per_sec=None,
                max_edge_dispatch_errors_per_sec=None,
                progress_every=1,
            )

            async def fake_http_request(
                method: str,
                url_text: str,
                headers: dict[str, str],
                body: bytes | None,
                connect_timeout_secs: float,
                request_timeout_secs: float,
            ) -> probe.HttpResponse:
                del method, url_text, headers, body, connect_timeout_secs, request_timeout_secs
                fake_http_request.calls += 1
                raw = "astra_capacity_run_slots_total 1\n"
                if fake_http_request.calls >= 2:
                    raw += (
                        'astra_durable_run_event_batches_total{path="streaming_terminal",'
                        'outcome="committed",compacted="false"} 1\n'
                    )
                return probe.HttpResponse(
                    status=200,
                    reason="OK",
                    headers={},
                    body=raw.encode("utf-8"),
                    header_latency_ms=1.0,
                )

            fake_http_request.calls = 0

            async def fake_stream_sse_request(**kwargs: object) -> probe.StreamResult:
                del kwargs
                await asyncio.sleep(0.01)
                return probe.StreamResult(
                    request_id=0,
                    user_index=0,
                    token_index=0,
                    http_status=200,
                    header_latency_ms=1.0,
                    first_event_ms=1.0,
                    duration_ms=10.0,
                    event_count=2,
                    session_id="session-1",
                    run_id="run-1",
                    terminal_status="completed",
                    error_code=None,
                    error_message=None,
                    retryable=None,
                    outcome="completed",
                )

            original_http_request = probe.http_request
            original_stream_sse_request = probe.stream_sse_request
            try:
                probe.http_request = fake_http_request
                probe.stream_sse_request = fake_stream_sse_request
                return await probe.run_probe(args)
            finally:
                probe.http_request = original_http_request
                probe.stream_sse_request = original_stream_sse_request

        with tempfile.TemporaryDirectory() as tmp:
            output_dir = Path(tmp)
            stdout = io.StringIO()
            stderr = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                exit_code = asyncio.run(run_case(output_dir))
            samples = [
                json.loads(line)
                for line in (output_dir / "metrics.jsonl").read_text(encoding="utf-8").splitlines()
            ]
            summary = json.loads((output_dir / "summary.json").read_text(encoding="utf-8"))

        self.assertEqual(exit_code, 0)
        self.assertGreaterEqual(len(samples), 2)
        self.assertEqual(samples[-1]["sample_kind"], "final")
        self.assertEqual(summary["metrics"]["durable_run_events"]["batches_last_total"], 1.0)
        self.assertEqual(summary["metrics"]["durable_run_events"]["batches_delta_total"], 1.0)

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
        self.assertEqual(
            summary["event_ingestion"]["dropped_before_acceptance_critical_total"],
            None,
        )
        self.assertEqual(summary["run_control"]["attempts_last_total"], None)
        self.assertEqual(summary["run_recovery"]["scans_last_total"], None)
        self.assertEqual(summary["ws_run_stream"]["attempts_last_total"], None)
        self.assertEqual(
            summary["control_plane"]["poll_attempts_per_worker_per_sec"],
            None,
        )
        self.assertEqual(
            summary["edge_dispatch"]["counters"]["claimed_total"]["last"],
            None,
        )

    def test_summarize_metrics_file_reports_event_ingestion_totals(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                json.dumps(
                    {
                        "unix_ms": 20,
                        "http_status": 200,
                        "metrics": {
                            "astra_event_ingestion_enqueue_overflows_total": 11.0,
                            "astra_event_ingestion_events_dropped_before_acceptance_total": 3.0,
                            'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="critical"}': 0.0,
                            'astra_event_ingestion_events_dropped_before_acceptance_by_priority_total{priority="telemetry"}': 3.0,
                            "astra_event_ingestion_events_dropped_permanent_total": 1.0,
                            "astra_event_ingestion_errors_total": 2.0,
                        },
                    }
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        self.assertEqual(
            summary["event_ingestion"],
            {
                "enqueue_overflows_total": 11.0,
                "dropped_before_acceptance_total": 3.0,
                "dropped_before_acceptance_critical_total": 0.0,
                "dropped_before_acceptance_telemetry_total": 3.0,
                "dropped_permanent_total": 1.0,
                "errors_total": 2.0,
            },
        )

    def test_summarize_metrics_file_reports_run_control_poll_deltas(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "unix_ms": 1_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_control_poll_attempts_total{operation="status",outcome="ok"}': 10.0,
                                    'astra_run_control_poll_attempts_total{operation="user_input_poll",outcome="ok"}': 3.0,
                                    'astra_run_control_poll_errors_total{operation="status",class="store"}': 1.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "unix_ms": 3_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_control_poll_attempts_total{operation="status",outcome="ok"}': 16.0,
                                    'astra_run_control_poll_attempts_total{operation="user_input_poll",outcome="ok"}': 5.0,
                                    'astra_run_control_poll_errors_total{operation="status",class="store"}': 2.0,
                                    'astra_run_control_poll_errors_total{operation="user_input_poll",class="missing"}': 4.0,
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        run_control = summary["run_control"]
        self.assertEqual(run_control["attempts_last_total"], 21.0)
        self.assertEqual(run_control["attempts_delta_total"], 8.0)
        self.assertEqual(run_control["attempts_per_sec"], 4.0)
        self.assertEqual(
            run_control["attempts_by_operation_outcome"]["status:ok"],
            {"last": 16.0, "delta": 6.0},
        )
        self.assertEqual(
            run_control["attempts_by_operation_outcome"]["user_input_poll:ok"],
            {"last": 5.0, "delta": 2.0},
        )
        self.assertEqual(run_control["errors_last_total"], 6.0)
        self.assertEqual(run_control["errors_delta_total"], 5.0)
        self.assertEqual(run_control["errors_per_sec"], 2.5)
        self.assertEqual(
            run_control["errors_by_operation_class"]["status:store"],
            {"last": 2.0, "delta": 1.0},
        )
        self.assertEqual(
            run_control["errors_by_operation_class"]["user_input_poll:missing"],
            {"last": 4.0, "delta": 4.0},
        )

    def test_summarize_metrics_file_reports_durable_run_event_error_deltas(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "unix_ms": 1_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_durable_run_event_batches_total{path="streaming_terminal",outcome="planned",compacted="false"}': 10.0,
                                    'astra_durable_run_event_batches_total{path="streaming_terminal",outcome="error",compacted="false"}': 1.0,
                                    'astra_durable_run_event_rows_total{path="streaming_terminal",outcome="error",compacted="false"}': 2.0,
                                    'astra_durable_run_event_bytes_total{path="streaming_terminal",outcome="error",compacted="false"}': 128.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "unix_ms": 3_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_durable_run_event_batches_total{path="streaming_terminal",outcome="planned",compacted="false"}': 15.0,
                                    'astra_durable_run_event_batches_total{path="streaming_terminal",outcome="error",compacted="false"}': 3.0,
                                    'astra_durable_run_event_rows_total{path="streaming_terminal",outcome="error",compacted="false"}': 6.0,
                                    'astra_durable_run_event_bytes_total{path="streaming_terminal",outcome="error",compacted="false"}': 384.0,
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        durable = summary["durable_run_events"]
        self.assertEqual(durable["batches_last_total"], 18.0)
        self.assertEqual(durable["batches_delta_total"], 7.0)
        self.assertEqual(
            durable["batches_by_path_outcome_compacted"]["streaming_terminal:error:false"],
            {"last": 3.0, "delta": 2.0},
        )
        self.assertEqual(durable["error_batches_delta_total"], 2.0)
        self.assertEqual(durable["error_batches_per_sec"], 1.0)
        self.assertEqual(durable["error_rows_delta_total"], 4.0)
        self.assertEqual(durable["error_bytes_delta_total"], 256.0)

    def test_summarize_metrics_file_reports_run_recovery_deltas(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "unix_ms": 1_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_recovery_scans_total{outcome="ok"}': 2.0,
                                    'astra_run_recovery_runs_total{action="preserve_waiting",outcome="ok"}': 3.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "unix_ms": 3_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_recovery_scans_total{outcome="ok"}': 3.0,
                                    'astra_run_recovery_scans_total{outcome="error"}': 1.0,
                                    'astra_run_recovery_runs_total{action="preserve_waiting",outcome="ok"}': 5.0,
                                    'astra_run_recovery_runs_total{action="fail_crashed",outcome="committed"}': 4.0,
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        run_recovery = summary["run_recovery"]
        self.assertEqual(run_recovery["scans_last_total"], 4.0)
        self.assertEqual(run_recovery["scans_delta_total"], 2.0)
        self.assertEqual(run_recovery["scans_per_sec"], 1.0)
        self.assertEqual(
            run_recovery["scans_by_outcome"]["ok"],
            {"last": 3.0, "delta": 1.0},
        )
        self.assertEqual(
            run_recovery["scans_by_outcome"]["error"],
            {"last": 1.0, "delta": 1.0},
        )
        self.assertEqual(run_recovery["runs_last_total"], 9.0)
        self.assertEqual(run_recovery["runs_delta_total"], 6.0)
        self.assertEqual(run_recovery["runs_per_sec"], 3.0)
        self.assertEqual(
            run_recovery["runs_by_action_outcome"]["preserve_waiting:ok"],
            {"last": 5.0, "delta": 2.0},
        )
        self.assertEqual(
            run_recovery["runs_by_action_outcome"]["fail_crashed:committed"],
            {"last": 4.0, "delta": 4.0},
        )

    def test_summarize_metrics_file_reports_ws_run_stream_poll_deltas(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "unix_ms": 1_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"}': 10.0,
                                    'astra_ws_run_stream_poll_attempts_total{operation="get_run_status",outcome="ok"}': 8.0,
                                    'astra_ws_run_stream_poll_errors_total{operation="stream_run",class="retryable"}': 1.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "unix_ms": 3_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"}': 14.0,
                                    'astra_ws_run_stream_poll_attempts_total{operation="get_run_status",outcome="ok"}': 11.0,
                                    'astra_ws_run_stream_poll_attempts_total{operation="get_run_status",outcome="error"}': 2.0,
                                    'astra_ws_run_stream_poll_errors_total{operation="stream_run",class="retryable"}': 2.0,
                                    'astra_ws_run_stream_poll_errors_total{operation="get_run_status",class="access_or_missing"}': 1.0,
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        ws_run_stream = summary["ws_run_stream"]
        self.assertEqual(ws_run_stream["attempts_last_total"], 27.0)
        self.assertEqual(ws_run_stream["attempts_delta_total"], 9.0)
        self.assertEqual(ws_run_stream["attempts_per_sec"], 4.5)
        self.assertEqual(
            ws_run_stream["attempts_by_operation_outcome"]["stream_run:ok"],
            {"last": 14.0, "delta": 4.0},
        )
        self.assertEqual(
            ws_run_stream["attempts_by_operation_outcome"]["get_run_status:error"],
            {"last": 2.0, "delta": 2.0},
        )
        self.assertEqual(ws_run_stream["errors_last_total"], 3.0)
        self.assertEqual(ws_run_stream["errors_delta_total"], 2.0)
        self.assertEqual(ws_run_stream["errors_per_sec"], 1.0)
        self.assertEqual(
            ws_run_stream["errors_by_operation_class"]["stream_run:retryable"],
            {"last": 2.0, "delta": 1.0},
        )
        self.assertEqual(
            ws_run_stream["errors_by_operation_class"]["get_run_status:access_or_missing"],
            {"last": 1.0, "delta": 1.0},
        )

    def test_summarize_metrics_file_reports_edge_dispatch_and_control_plane_capacity(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "metrics.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "unix_ms": 1_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_control_poll_attempts_total{operation="status",outcome="ok"}': 2.0,
                                    'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"}': 4.0,
                                    "astra_edge_dispatch_pending_rows": 3.0,
                                    "astra_edge_dispatch_claimed_total": 10.0,
                                    "astra_edge_dispatch_deliver_misses_total": 1.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "unix_ms": 3_000,
                                "http_status": 200,
                                "metrics": {
                                    'astra_run_control_poll_attempts_total{operation="status",outcome="ok"}': 8.0,
                                    'astra_ws_run_stream_poll_attempts_total{operation="stream_run",outcome="ok"}': 10.0,
                                    "astra_edge_dispatch_pending_rows": 5.0,
                                    "astra_edge_dispatch_claimed_total": 18.0,
                                    "astra_edge_dispatch_deliver_misses_total": 3.0,
                                    "astra_edge_dispatch_wait_result_timeouts_total": 2.0,
                                },
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            summary = probe.summarize_metrics_file(path)

        self.assertEqual(summary["edge_dispatch"]["gauges_last"]["pending_rows"], 5.0)
        self.assertEqual(
            summary["edge_dispatch"]["counters"]["claimed_total"],
            {"last": 18.0, "delta": 8.0, "per_sec": 4.0},
        )
        self.assertEqual(summary["edge_dispatch"]["error_events_delta_total"], 4.0)
        self.assertEqual(summary["edge_dispatch"]["error_events_per_sec"], 2.0)
        self.assertEqual(summary["control_plane"]["poll_attempts_per_sec"], 6.0)
        self.assertEqual(summary["control_plane"]["poll_attempts_per_worker_per_sec"], None)

        summary["control_plane"] = probe.summarize_control_plane_metrics(summary, worker_count=3)
        self.assertEqual(summary["control_plane"]["poll_attempts_per_worker_per_sec"], 2.0)

    def test_evaluate_capacity_contracts_reports_poll_and_edge_budget_violations(self) -> None:
        args = argparse.Namespace(
            max_control_plane_polls_per_worker_per_sec=1.5,
            max_edge_dispatch_errors_per_sec=0.5,
            require_no_run_control_errors=True,
            require_no_durable_event_errors=True,
            require_no_edge_dispatch_errors=True,
        )
        metrics_summary = {
            "control_plane": {"poll_attempts_per_worker_per_sec": 2.0},
            "run_control": {"errors_delta_total": 3.0},
            "durable_run_events": {"error_batches_delta_total": 2.0},
            "edge_dispatch": {"error_events_per_sec": 1.0, "error_events_delta_total": 4.0},
        }

        self.assertEqual(
            probe.evaluate_capacity_contracts(args, metrics_summary),
            [
                "control_plane_poll_attempts_per_worker_per_sec:2>1.5",
                "run_control_errors:3",
                "durable_run_event_error_batches:2",
                "edge_dispatch_error_events_per_sec:1>0.5",
                "edge_dispatch_error_events:4",
            ],
        )

    def test_evaluate_capacity_contracts_ignores_unobserved_or_disabled_budgets(self) -> None:
        args = argparse.Namespace(
            max_control_plane_polls_per_worker_per_sec=None,
            max_edge_dispatch_errors_per_sec=None,
            require_no_run_control_errors=False,
            require_no_durable_event_errors=False,
            require_no_edge_dispatch_errors=False,
        )
        metrics_summary = {
            "control_plane": {"poll_attempts_per_worker_per_sec": 100.0},
            "run_control": {"errors_delta_total": 10.0},
            "durable_run_events": {"error_batches_delta_total": 10.0},
            "edge_dispatch": {"error_events_per_sec": 10.0},
        }

        self.assertEqual(probe.evaluate_capacity_contracts(args, metrics_summary), [])

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
