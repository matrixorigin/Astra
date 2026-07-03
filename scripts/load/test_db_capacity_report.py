#!/usr/bin/env python3
"""Unit tests for db_capacity_report.py."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("db_capacity_report.py")
SPEC = importlib.util.spec_from_file_location("db_capacity_report", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
reporter = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = reporter
SPEC.loader.exec_module(reporter)


def probe_summary(
    *,
    total: int = 300,
    completed: int = 300,
    failed: int = 0,
    error_codes: dict[str, int] | None = None,
    contract_violations: list[str] | None = None,
    failure_reasons: dict[str, int] | None = None,
    outcomes: dict[str, int] | None = None,
) -> dict[str, object]:
    return {
        "base_url": "http://127.0.0.1:17001",
        "profile": "500-cli" if total >= 500 else "300-cli",
        "total": total,
        "completed": completed,
        "failed": failed,
        "error_codes": error_codes or {},
        "contract_violations": contract_violations or [],
        "failure_reasons": failure_reasons or {},
        "outcomes": outcomes or {"completed": completed},
    }


def slow_summary(
    *,
    slow_sql_count: int = 0,
    pool_wait_count: int = 0,
    pool_wait_by_kind: dict[str, int] | None = None,
    slow_sql_by_summary: dict[str, int] | None = None,
    top_elapsed_secs: float = 0.0,
) -> dict[str, object]:
    top_slow_sql = []
    if top_elapsed_secs > 0:
        top_slow_sql.append({"elapsed_secs": top_elapsed_secs, "summary": "COMMIT"})
    return {
        "slow_sql_count": slow_sql_count,
        "pool_wait_count": pool_wait_count,
        "pool_wait_by_kind": pool_wait_by_kind or {},
        "slow_sql_by_summary": slow_sql_by_summary or {},
        "top_slow_sql": top_slow_sql,
    }


class DbCapacityReportTests(unittest.TestCase):
    def test_clean_sample_is_release_safe(self) -> None:
        report = reporter.build_report(
            probe_summary(total=300, completed=300),
            slow_summary(slow_sql_count=11),
            reporter.Thresholds(),
        )

        self.assertEqual(report["verdict"], "release_safe_capacity_sample")
        self.assertTrue(report["release_safe"])
        self.assertEqual(report["failure_modes"], [])

    def test_pool_timeouts_and_broad_slowdown_are_db_saturation(self) -> None:
        report = reporter.build_report(
            probe_summary(
                total=500,
                completed=454,
                failed=46,
                error_codes={"run_admission_timeout": 46},
                contract_violations=[
                    "run_control_errors:86",
                    "edge_dispatch_error_events:5",
                ],
            ),
            slow_summary(
                slow_sql_count=205,
                pool_wait_count=59,
                pool_wait_by_kind={"acquire_slow": 55, "pool_timeout": 4},
                slow_sql_by_summary={
                    "SELECT run_id, user_id, session_id, ...": 54,
                    "COMMIT": 32,
                    "UPDATE run_display_projections SET status ...": 32,
                    "UPDATE agent_sessions SET event_count ...": 29,
                },
                top_elapsed_secs=50.3,
            ),
            reporter.Thresholds(),
        )

        self.assertEqual(report["verdict"], "db_saturation_boundary")
        self.assertFalse(report["release_safe"])
        self.assertIn("db_saturation", report["failure_modes"])
        self.assertIn("admission_limited", report["failure_modes"])
        self.assertTrue(report["evidence"]["broad_slowdown"])

    def test_network_failure_without_db_pressure_is_harness_failure(self) -> None:
        report = reporter.build_report(
            probe_summary(
                total=300,
                completed=0,
                failed=300,
                failure_reasons={"network:connection closed": 300},
                outcomes={"network": 300},
            ),
            slow_summary(),
            reporter.Thresholds(),
        )

        self.assertEqual(report["verdict"], "external_or_harness_failure")
        self.assertIn("external_or_harness_failure_suspected", report["failure_modes"])
        self.assertFalse(report["release_safe"])

    def test_admission_timeout_is_not_harness_failure(self) -> None:
        report = reporter.build_report(
            probe_summary(
                total=500,
                completed=450,
                failed=50,
                error_codes={"run_admission_timeout": 50},
                failure_reasons={"error_code:run_admission_timeout": 50},
            ),
            slow_summary(),
            reporter.Thresholds(),
        )

        self.assertEqual(report["verdict"], "admission_limited")
        self.assertIn("admission_limited", report["failure_modes"])
        self.assertNotIn("external_or_harness_failure_suspected", report["failure_modes"])

    def test_require_release_safe_returns_nonzero_for_saturation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            probe_path = tmp_path / "summary.json"
            slow_path = tmp_path / "slow-sql-summary.json"
            probe_path.write_text(
                json.dumps(
                    probe_summary(
                        total=500,
                        completed=454,
                        failed=46,
                        error_codes={"run_admission_timeout": 46},
                    )
                ),
                encoding="utf-8",
            )
            slow_path.write_text(
                json.dumps(
                    slow_summary(
                        slow_sql_count=120,
                        pool_wait_count=1,
                        pool_wait_by_kind={"pool_timeout": 1},
                    )
                ),
                encoding="utf-8",
            )

            result = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--probe-summary",
                    str(probe_path),
                    "--slow-sql-summary",
                    str(slow_path),
                    "--require-release-safe",
                ],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

        self.assertEqual(result.returncode, 1)
        self.assertIn("db_saturation_boundary", result.stdout)


if __name__ == "__main__":
    unittest.main()
