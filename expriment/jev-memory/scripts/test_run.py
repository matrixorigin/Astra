"""Verify safe defaults and rejection before any provider call."""
import contextlib
import io
import json
from pathlib import Path
import unittest
import tempfile
import subprocess
from unittest.mock import patch

import run


class RunnerTest(unittest.TestCase):
    def test_default_is_offline_and_root_is_repository(self):
        args = run.parse_args([])
        self.assertFalse(args.live)
        self.assertIsNone(args.models)
        self.assertEqual(args.repeat, 3)
        self.assertTrue((run.REPO / "Cargo.toml").is_file())

    def test_live_without_credentials_stops_before_commands(self):
        with patch.object(run, "command") as command, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                run.main(["--live"])
            command.assert_not_called()

    def test_offline_never_runs_binary_or_creates_live_output(self):
        with patch.object(run, "command") as command, patch.object(run.tempfile, "mkdtemp") as create:
            with contextlib.redirect_stdout(io.StringIO()):
                run.main([])
            self.assertEqual(command.call_count, 3)
            create.assert_not_called()
            for call in command.call_args_list:
                self.assertNotIn("--ignored", call.args[0])
                self.assertNotIn("env", call.kwargs)

    def test_live_orchestration_uses_exact_binary_and_analyzes_every_suite(self):
        with tempfile.TemporaryDirectory() as directory:
            # The mock never reads the credential file; use this existing source as a path.
            build = subprocess.CompletedProcess([], 0, stdout=json.dumps({
                "reason": "compiler-artifact", "executable": "/mock/runtime-tests",
                "profile": {"test": True}}))
            with patch.object(run, "command", return_value=build) as command, \
                    patch.object(run.tempfile, "mkdtemp", return_value=directory), \
                    contextlib.redirect_stdout(io.StringIO()):
                run.main(["--live", "--models", __file__])
            calls = command.call_args_list
            live = [c for c in calls if "env" in c.kwargs]
            self.assertEqual(len(live), 4)
            for call in live:
                self.assertEqual(call.args[0][:2], ["/mock/runtime-tests", run.TEST])
                self.assertIn("--exact", call.args[0])
                self.assertEqual(call.kwargs["env"]["ASTRA_MEMORY_EVAL_REPEAT"], "3")
            reports = [c for c in calls if any(str(x).endswith("summary-official.json") for x in c.args[0])]
            self.assertEqual(len(reports), 4)


if __name__ == "__main__":
    unittest.main()
