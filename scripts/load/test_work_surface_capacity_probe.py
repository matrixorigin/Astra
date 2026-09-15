#!/usr/bin/env python3
"""Unit tests for work_surface_capacity_probe.py."""

from __future__ import annotations

import argparse
import asyncio
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


ROOT = Path(__file__).parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
SCRIPT = ROOT / "work_surface_capacity_probe.py"
SPEC = importlib.util.spec_from_file_location("work_surface_capacity_capacity_probe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)


class WorkSurfaceCapacityProbeTests(unittest.TestCase):
    def test_schedule_covers_each_owner_and_session(self) -> None:
        requests = [probe.schedule_request(i, 100, 25, 4, 25) for i in range(100)]
        self.assertEqual(
            {(request.owner_index, request.session_index) for request in requests},
            {(owner, session) for owner in range(25) for session in range(4)},
        )
        self.assertEqual(sum(request.active_view_index is not None for request in requests), 100)

    def test_render_identifier_supports_all_probe_dimensions(self) -> None:
        request = probe.schedule_request(126, 100, 25, 4, 25)
        self.assertEqual(
            probe.render_identifier(
                "work-{owner_index}-{session_index}-{reader_index}-{round_index}", request
            ),
            "work-1-1-26-1",
        )

    def test_render_identifier_rejects_path_injection(self) -> None:
        request = probe.schedule_request(0, 1, 1, 1, 1)
        with self.assertRaises(probe.ProbeError):
            probe.render_identifier("work/{owner_index}", request)

    def test_endpoint_quotes_identifiers(self) -> None:
        self.assertEqual(
            probe.endpoint_url("http://api.test", "work id", "branch#1"),
            "http://api.test/v1/works/work%20id/branches/branch%231/execution",
        )

    def test_validate_args_requires_templates_and_tokens_for_live_run(self) -> None:
        args = argparse.Namespace(
            profile="work-smoke",
            owners=1,
            sessions_per_owner=1,
            readers=1,
            active_views=1,
            rounds=1,
            concurrency=1,
            connect_timeout_secs=1.0,
            request_timeout_secs=1.0,
            poll_interval_secs=1.0,
            max_p95_ms=None,
            max_p99_ms=None,
            max_body_bytes=1024,
            dry_run=False,
            work_id_template=None,
            branch_id_template="branch",
            tokens=[],
            require_distinct_users=False,
            check_owner_isolation=False,
        )
        with self.assertRaises(probe.ProbeError):
            probe.validate_args(args)

    def test_validate_args_rejects_unverifiable_isolation_profile(self) -> None:
        args = argparse.Namespace(
            profile="work-smoke",
            owners=2,
            sessions_per_owner=2,
            readers=3,
            active_views=2,
            rounds=1,
            concurrency=2,
            connect_timeout_secs=1.0,
            request_timeout_secs=1.0,
            poll_interval_secs=1.0,
            max_p95_ms=None,
            max_p99_ms=None,
            max_body_bytes=1024,
            dry_run=False,
            work_id_template="work-{owner_index}-{session_index}",
            branch_id_template="branch-{owner_index}-{session_index}",
            tokens=["owner-a", "owner-b"],
            require_distinct_users=True,
            check_owner_isolation=True,
        )
        with self.assertRaises(probe.ProbeError) as context:
            probe.validate_args(args)
        self.assertIn("successful Work read per owner/Session pair", str(context.exception))

    def test_isolation_pair_requires_targets_proven_by_successful_reads(self) -> None:
        args = argparse.Namespace(
            owners=2,
            sessions_per_owner=1,
            rounds=1,
            tokens=["owner-a", "owner-b"],
        )
        requests, violations = probe.build_isolation_requests(
            args,
            {(0, 0): ("work-0", "branch-0")},
            2,
        )
        self.assertEqual(requests, [])
        self.assertEqual(violations, ["owner_isolation_targets_missing:0/2"])


class WorkSurfaceCapacityProbeAsyncTests(unittest.IsolatedAsyncioTestCase):
    async def test_read_sends_work_api_version_header(self) -> None:
        seen: dict[str, str] = {}

        async def fake_http_request(
            method: str,
            url: str,
            headers: dict[str, str],
            body: bytes | None,
            connect_timeout_secs: float,
            request_timeout_secs: float,
        ) -> SimpleNamespace:
            del url, body, connect_timeout_secs, request_timeout_secs
            seen.update({"method": method, **headers})
            return SimpleNamespace(
                status=200,
                body=json.dumps(
                    {"work_id": "work-0", "branch_id": "branch-0", "generation": 1}
                ).encode(),
            )

        args = argparse.Namespace(
            base_url="http://api.test",
            connect_timeout_secs=1.0,
            request_timeout_secs=1.0,
            max_body_bytes=1024,
        )
        request = probe.schedule_request(0, 1, 1, 1, 1)
        original = probe.http_request
        try:
            probe.http_request = fake_http_request
            result = await probe.read_work_execution(
                args, request, "token", "work-0", "branch-0"
            )
        finally:
            probe.http_request = original
        self.assertEqual(result.outcome, "completed")
        self.assertEqual(seen["method"], "GET")
        self.assertEqual(seen[probe.WORK_API_MAJOR_HEADER], probe.WORK_API_MAJOR)

    async def test_isolation_checks_respect_read_concurrency_budget(self) -> None:
        args = argparse.Namespace(concurrency=2, tokens=["owner-a", "owner-b"])
        requests = [
            (
                probe.WorkRequest(index, index, 1, index % 2, 0, None),
                "owner-a",
                f"foreign-work-{index}",
                f"foreign-branch-{index}",
            )
            for index in range(5)
        ]
        active = 0
        peak = 0

        async def fake_read(*_args: object, **_kwargs: object) -> probe.WorkResult:
            nonlocal active, peak
            active += 1
            peak = max(peak, active)
            await asyncio.sleep(0.005)
            active -= 1
            request = _args[1]
            assert isinstance(request, probe.WorkRequest)
            return probe.WorkResult(
                request_id=request.request_id,
                reader_index=request.reader_index,
                round_index=request.round_index,
                owner_index=request.owner_index,
                session_index=request.session_index,
                token_index=None,
                http_status=404,
                latency_ms=1.0,
                body_bytes=0,
                outcome="owner_isolation_pass",
            )

        with tempfile.TemporaryDirectory() as directory:
            writer = probe.ResultWriter(Path(directory))
            original = probe.read_work_execution
            try:
                probe.read_work_execution = fake_read
                results = await probe.run_isolation_checks(args, writer, requests)
            finally:
                probe.read_work_execution = original
                writer.close()
        self.assertEqual(len(results), 5)
        self.assertLessEqual(peak, args.concurrency)


if __name__ == "__main__":
    unittest.main()
