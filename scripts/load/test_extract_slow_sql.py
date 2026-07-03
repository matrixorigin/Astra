#!/usr/bin/env python3
"""Unit tests for extract_slow_sql.py."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("extract_slow_sql.py")
SPEC = importlib.util.spec_from_file_location("extract_slow_sql", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
extractor = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = extractor
SPEC.loader.exec_module(extractor)


class ExtractSlowSqlTests(unittest.TestCase):
    def test_build_report_extracts_slow_sql_and_pool_waits(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log_path = Path(tmp) / "api_server.log"
            log_path.write_text(
                "\n".join(
                    [
                        "plain startup line",
                        json.dumps(
                            {
                                "timestamp": "2026-07-03T05:53:00Z",
                                "level": "WARN",
                                "target": "sqlx::query",
                                "fields": {
                                    "message": extractor.SLOW_SQL_MESSAGE,
                                    "summary": "SELECT old",
                                    "db.statement": "SELECT 1",
                                    "elapsed_secs": 9.0,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "timestamp": "2026-07-03T05:54:00Z",
                                "level": "WARN",
                                "target": "sqlx::query",
                                "fields": {
                                    "message": extractor.SLOW_SQL_MESSAGE,
                                    "summary": "UPDATE agent_runs SET total_prompt_tokens ...",
                                    "db.statement": "\nUPDATE agent_runs\n SET total_prompt_tokens = ?\n",
                                    "elapsed_secs": 30.5,
                                    "rows_affected": 1,
                                    "rows_returned": 0,
                                },
                                "span": {
                                    "request_id": "req-1",
                                    "http.route": "/chat/stream",
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "timestamp": "2026-07-03T05:54:01Z",
                                "level": "WARN",
                                "target": "sqlx::pool::acquire",
                                "fields": {
                                    "message": (
                                        "acquired connection, but time to acquire exceeded slow threshold"
                                    ),
                                    "aquired_after_secs": 2.5,
                                },
                            }
                        ),
                        json.dumps(
                            {
                                "timestamp": "2026-07-03T05:54:02Z",
                                "level": "ERROR",
                                "target": "astra.agent",
                                "fields": {
                                    "message": (
                                        "failed to persist core events: "
                                        "pool timed out while waiting for an open connection"
                                    ),
                                },
                                "spans": [{"request_id": "req-2", "http.route": "/chat/stream"}],
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            report = extractor.build_report(
                log_path,
                extractor.parse_timestamp("2026-07-03T05:53:59Z"),
                extractor.parse_timestamp("2026-07-03T05:54:03Z"),
                min_elapsed_secs=1.0,
            )
            output_dir = Path(tmp) / "out"
            extractor.write_report(report, output_dir, top=10)

            slow_tsv = (output_dir / "slow-sql.tsv").read_text(encoding="utf-8")
            pool_tsv = (output_dir / "db-pool-waits.tsv").read_text(encoding="utf-8")
            summary = json.loads((output_dir / "slow-sql-summary.json").read_text(encoding="utf-8"))

        self.assertEqual(report.skipped_lines, 1)
        self.assertEqual(len(report.slow_sql_rows), 1)
        self.assertEqual(len(report.pool_wait_rows), 2)
        self.assertIn("UPDATE agent_runs SET total_prompt_tokens = ?", slow_tsv)
        self.assertIn("req-1", slow_tsv)
        self.assertIn("acquire_slow", pool_tsv)
        self.assertIn("pool_timeout", pool_tsv)
        self.assertEqual(summary["slow_sql_count"], 1)
        self.assertEqual(summary["pool_wait_by_kind"], {"acquire_slow": 1, "pool_timeout": 1})

    def test_parse_timestamp_rejects_invalid_input(self) -> None:
        self.assertIsNone(extractor.parse_timestamp("not-a-time"))


if __name__ == "__main__":
    unittest.main()
