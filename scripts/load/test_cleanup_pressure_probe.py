#!/usr/bin/env python3
"""Unit tests for cleanup_pressure_probe.py."""

from __future__ import annotations

import argparse
import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("cleanup_pressure_probe.py")
SPEC = importlib.util.spec_from_file_location("cleanup_pressure_probe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)


class CleanupPressureProbeTests(unittest.TestCase):
    def test_parse_pressure_results_extracts_json_lines(self) -> None:
        results = probe.parse_pressure_results(
            'noise\nCLEANUP_PRESSURE_RESULT {"path":"queue","rows_deleted":5}\nmore\n'
        )
        self.assertEqual(results, [{"path": "queue", "rows_deleted": 5}])

    def test_safe_database_name_requires_test_or_smoke(self) -> None:
        self.assertEqual(probe.safe_database_name("astra_test_cleanup"), "astra_test_cleanup")
        with self.assertRaises(ValueError):
            probe.safe_database_name("prod")
        with self.assertRaises(ValueError):
            probe.safe_database_name("astra-test")

    def test_build_commands_sets_rows_and_nocapture(self) -> None:
        args = argparse.Namespace(
            profile="smoke",
            database_base="astra_runtime_test_cleanup_pressure",
            queue_rows=3001,
            csl_rows=4001,
            prompt_rows=5001,
            prompt_keep_rows=77,
        )
        commands = probe.build_commands(args)
        self.assertEqual([command.name for command in commands], [
            "agent_message_queue",
            "conversation_log",
            "prompt_retention",
        ])
        self.assertEqual(commands[0].env["ASTRA_CLEANUP_PRESSURE_QUEUE_ROWS"], "3001")
        self.assertEqual(commands[1].env["ASTRA_CLEANUP_PRESSURE_CSL_ROWS"], "4001")
        self.assertEqual(commands[2].env["ASTRA_CLEANUP_PRESSURE_PROMPT_ROWS"], "5001")
        self.assertEqual(commands[2].env["ASTRA_CLEANUP_PRESSURE_PROMPT_KEEP_ROWS"], "77")
        for command in commands:
            self.assertIn("--nocapture", command.command)
            self.assertIn("test", command.database)

    def test_build_summary_marks_any_failed_command_not_ok(self) -> None:
        args = argparse.Namespace(profile="smoke", database_base="astra_test_cleanup")
        summary = probe.build_summary(
            args,
            [
                {"name": "a", "returncode": 0},
                {"name": "b", "returncode": 1},
            ],
        )
        self.assertFalse(summary["ok"])


if __name__ == "__main__":
    unittest.main()
