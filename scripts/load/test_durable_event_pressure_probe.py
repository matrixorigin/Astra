#!/usr/bin/env python3
"""Unit tests for durable_event_pressure_probe.py."""

from __future__ import annotations

import argparse
import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("durable_event_pressure_probe.py")
SPEC = importlib.util.spec_from_file_location("durable_event_pressure_probe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)


class DurableEventPressureProbeTests(unittest.TestCase):
    def test_parse_pressure_results_extracts_json_lines(self) -> None:
        results = probe.parse_pressure_results(
            'noise\nDURABLE_EVENT_PRESSURE_RESULT {"runs":3,"total_persisted_rows":1503}\n'
        )
        self.assertEqual(results, [{"runs": 3, "total_persisted_rows": 1503}])

    def test_safe_database_name_requires_test_or_smoke(self) -> None:
        self.assertEqual(
            probe.safe_database_name("astra_runtime_test_durable_event_pressure"),
            "astra_runtime_test_durable_event_pressure",
        )
        with self.assertRaises(ValueError):
            probe.safe_database_name("prod")
        with self.assertRaises(ValueError):
            probe.safe_database_name("astra-test")

    def test_build_command_sets_env_and_uses_lib_filter_without_exact(self) -> None:
        args = argparse.Namespace(
            profile="smoke",
            database="astra_runtime_test_durable_event_pressure",
            runs=7,
            text_deltas=1111,
            progress_rows=555,
        )
        command = probe.build_command(args)
        self.assertEqual(command.name, "durable_event_pressure")
        self.assertEqual(command.env["ASTRA_DURABLE_EVENT_PRESSURE_RUNS"], "7")
        self.assertEqual(command.env["ASTRA_DURABLE_EVENT_PRESSURE_TEXT_DELTAS"], "1111")
        self.assertEqual(command.env["ASTRA_DURABLE_EVENT_PRESSURE_PROGRESS_ROWS"], "555")
        self.assertIn("durable_run_event_pressure_probe", command.command)
        self.assertIn("--lib", command.command)
        self.assertIn("--nocapture", command.command)
        self.assertNotIn("--exact", command.command)

    def test_build_summary_requires_one_machine_readable_result(self) -> None:
        args = argparse.Namespace(
            profile="smoke",
            database="astra_runtime_test_durable_event_pressure",
        )
        self.assertTrue(
            probe.build_summary(
                args,
                {"returncode": 0, "results": [{"runs": 3}]},
            )["ok"]
        )
        self.assertFalse(
            probe.build_summary(
                args,
                {"returncode": 0, "results": []},
            )["ok"]
        )
        self.assertFalse(
            probe.build_summary(
                args,
                {"returncode": 1, "results": [{"runs": 3}]},
            )["ok"]
        )


if __name__ == "__main__":
    unittest.main()
