"""Ensure synthetic compatibility diagnostics do not expose secrets or locations."""

import unittest
from subprocess import CompletedProcess
from pathlib import Path
from argparse import Namespace
import json
import subprocess
import sys
import tempfile
from unittest.mock import patch

from client_compatibility import Failure, identify_current, paired_tool_result, result_json, run, safe_error_summary


class CompatibilityDiagnosticsTests(unittest.TestCase):
    def test_wrong_server_is_rejected_before_bootstrap(self):
        args = Namespace(api_url="http://127.0.0.1:17001")
        with patch("client_compatibility.request", return_value={"build_git_sha": "wrong"}), \
             patch("client_compatibility.Provider") as provider, \
             self.assertRaisesRegex(Failure, "candidate_server_source_mismatch"):
            run(args, {"server_source": "source"})
        provider.assert_not_called()

    def test_wrong_cli_is_rejected_before_journeys(self):
        build = CompletedProcess([], 0, stdout=json.dumps({"git_sha": "wrong"}).encode(), stderr=b"")
        with patch("client_compatibility.command", return_value=build) as command, \
             self.assertRaisesRegex(Failure, "candidate_cli_source_mismatch"):
            identify_current(Path("/not-used"), {}, Path("/not-used"), {"server_source": "source"})
        command.assert_called_once()

    def test_candidate_driver_requires_both_client_binaries(self):
        driver = Path(__file__).with_name("client_compatibility.py")
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "report.json"
            for missing in ("--current-cli", "--current-edge"):
                supplied = "--current-edge" if missing == "--current-cli" else "--current-cli"
                result = subprocess.run(
                    [sys.executable, str(driver), "--api-url", "http://127.0.0.1:1",
                     "--report", str(report), "--disposable-server", supplied, "/missing"],
                    capture_output=True, check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(missing.encode(), result.stderr)
                self.assertFalse(report.exists())

    def test_errors_keep_the_reason_but_redact_credentials_and_locations(self):
        secret = "fixture-secret-value-which-must-never-be-retained"
        message = (
            "API error (422): unsupported request field; "
            f'Authorization: Bearer {secret}; password="short-secret"; '
            '"api_key": "short-key"; '
            "https://fixture.invalid/private?token=hidden; "
            "/home/fixture/private-file; C:\\fixture\\private-file; "
            "eyJhbGciOiJub25lIn0.eyJzdWIiOiJmaXh0dXJlIn0.signature"
        )
        summary = safe_error_summary(message)
        self.assertIn("API error (422): unsupported request field", summary)
        for forbidden in (
            secret, "short-secret", "short-key", "fixture.invalid", "private-file",
            "eyJhbGciOiJub25lIn0", "eyJzdWIiOiJmaXh0dXJlIn0",
        ):
            self.assertNotIn(forbidden, summary)

    def test_summary_is_bounded_and_does_not_accept_structured_raw_responses(self):
        self.assertIsNone(safe_error_summary({"error": "untrusted body"}))
        self.assertLessEqual(len(safe_error_summary("safe diagnostic " * 300)), 1024)

    def test_non_json_preflight_failure_keeps_only_redacted_diagnostics(self):
        result = CompletedProcess([], 2, stdout=b"", stderr=(
            b"resume bundle schema 9 is unsupported; password=synthetic-secret "
            b"https://fixture.invalid/private /home/fixture/private-file"))
        with self.assertRaises(Failure) as caught:
            result_json(result)
        self.assertEqual(str(caught.exception), "client_terminal_invalid")
        diagnostics = caught.exception.diagnostics
        self.assertEqual(diagnostics["exit_code"], 2)
        self.assertNotIn("stdout_summary", diagnostics)
        self.assertIn("resume bundle schema 9 is unsupported", diagnostics["stderr_summary"])
        for forbidden in ("synthetic-secret", "fixture.invalid", "private-file"):
            self.assertNotIn(forbidden, diagnostics["stderr_summary"])


class CompatibilityToolHistoryTests(unittest.TestCase):
    def test_runtime_user_suffix_does_not_hide_the_current_tool_result(self):
        call_id = "compat-operation-current-case"
        declaration = {"role": "assistant", "tool_calls": [
            {"id": call_id, "function": {"name": "write_file", "arguments": "{}"}}]}
        result = {"role": "tool", "tool_call_id": call_id, "content": "synthetic result"}
        history = [{"role": "user", "content": "synthetic task"},
                   {"role": "assistant", "content": "synthetic prelude", "tool_calls": None}, declaration, result,
                   {"role": "user", "content": "runtime context suffix"}]
        self.assertEqual(paired_tool_result(history, call_id, "write_file"), result)
        self.assertIsNone(paired_tool_result(history, "compat-operation-other-case", "write_file"))
        self.assertIsNone(paired_tool_result(history, call_id, "read_file"))
        self.assertIsNone(paired_tool_result([result], call_id, "write_file"))
        self.assertIsNone(paired_tool_result([declaration, result, result], call_id, "write_file"))
        self.assertIsNone(paired_tool_result([result, declaration], call_id, "write_file"))
        conflicting = {"role": "assistant", "tool_calls": [
            {"id": call_id, "function": {"name": "read_file", "arguments": "{}"}}]}
        self.assertIsNone(paired_tool_result([declaration, conflicting, result], call_id, "write_file"))


if __name__ == "__main__":
    unittest.main()
