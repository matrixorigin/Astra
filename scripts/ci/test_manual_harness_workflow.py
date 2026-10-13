"""Exercise the workflow's actual Python blocks without provider calls or builds."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/harness-manual.yml"


def python_block(step):
    section = WORKFLOW.read_text().split(f"- name: {step}\n", 1)[1]
    block = re.search(r"python3 - <<'PY'\n(.*?)\n          PY", section, re.S)[1]
    return "\n".join(line[10:] for line in block.splitlines())


class ManualHarnessWorkflowTests(unittest.TestCase):
    def run_block(self, step, root, **env):
        return subprocess.run(
            ["python3", "-c", python_block(step)], cwd=root, capture_output=True,
            text=True, env={**os.environ, "RUNNER_TEMP": str(root), **env}, timeout=10,
        )

    def test_private_config_and_secret_failures(self):
        step = "Validate secrets and prepare private model file"
        for url, key in [("", "sentinel-secret"), ("https://example.invalid/v1", ""),
                         ("http://example.invalid/v1", "sentinel-secret"),
                         ("https://user:sentinel-secret@example.invalid/v1", "sentinel-secret")]:
            with self.subTest(url=url), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                result = self.run_block(step, root, GLM_BASE_URL=url, GLM_API_KEY=key)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("sentinel-secret", result.stdout + result.stderr)
                self.assertFalse((root / "harness/models.yaml").exists())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            result = self.run_block(step, root, GLM_BASE_URL="https://example.invalid/v1",
                                    GLM_API_KEY="sentinel-secret")
            self.assertEqual(result.returncode, 0, result.stderr)
            path = root / "harness/models.yaml"
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            model = json.loads(path.read_text())[0]
            self.assertEqual(model["api_key"], "sentinel-secret")
            self.assertEqual(model["thinking_protocol"], "reasoning_effort")
            self.assertNotIn("sentinel-secret", result.stdout + result.stderr)

    def test_summary_excludes_raw_evidence_and_preserves_no_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            summary = root / "job-summary"
            env = {"GITHUB_STEP_SUMMARY": str(summary), "GITHUB_SHA": "a" * 40}
            result = self.run_block("Publish safe result summary", root, **env)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("not a pass", summary.read_text())
            metrics = ("duration_ms", "ttft_ms", "turn_rounds", "tool_calls_count",
                       "prompt_tokens", "cached_input_tokens", "cache_creation_tokens", "completion_tokens")
            outcome = {**dict.fromkeys(metrics, 7), "exit_code": 1,
                       "text": "sentinel-secret", "stderr": "sentinel-secret",
                       "explain_capture": {"private": "sentinel-secret"}}
            report = {"runs": [{"case_name": "hello_text_contains", "status": "failed",
                                "run_index": 0, "outcome": outcome,
                                "failure_class": {"InfraProviderError": {"provider": "sentinel-secret"}},
                                "criteria": [{"criterion": {"type": "text_contains", "needle": "sentinel-secret"},
                                              "passed": False, "message": "sentinel-secret"}]}]}
            (root / "harness/report.json").write_text(json.dumps(report))
            result = self.run_block("Publish safe result summary", root, **env)
            self.assertEqual(result.returncode, 0, result.stderr)
            safe = (root / "harness/summary.json").read_text()
            self.assertNotIn("sentinel-secret", safe + summary.read_text())
            row = json.loads(safe)["runs"][0]
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["failure_class"], "InfraProviderError")
            self.assertEqual(row["criteria"], [{"type": "text_contains", "passed": False}])
            self.assertEqual(row["terminal_reported"]["cached_input_tokens"], 7)
            self.assertIsNone(row["token_usage_coverage"])

    def test_failure_diagnostics_project_only_safe_model_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            (root / "harness/model-status.json").write_text(json.dumps({
                "model_id": "model-id", "is_active": False,
                "base_url": "sentinel-secret", "connectivity": "sentinel-secret",
            }))
            result = self.run_block("Diagnose failure without exposing provider responses", root,
                                    GITHUB_STEP_SUMMARY=str(root / "job-summary"),
                                    DIAGNOSTIC_HEALTH_STATUS="200", DIAGNOSTIC_MODEL_EXIT="0",
                                    MODEL_LOAD_OUTCOME="failure", HARNESS_OUTCOME="skipped")
            self.assertEqual(result.returncode, 0, result.stderr)
            text = (root / "harness/diagnostic.json").read_text()
            self.assertNotIn("sentinel-secret", text + (root / "job-summary").read_text())
            diagnostic = json.loads(text)
            self.assertTrue(diagnostic["model_registered"])
            self.assertFalse(diagnostic["model_active"])
            self.assertEqual(diagnostic["harness_step"], "skipped")
            (root / "harness/model-status.json").unlink()
            result = self.run_block("Diagnose failure without exposing provider responses", root,
                                    GITHUB_STEP_SUMMARY=str(root / "job-summary"),
                                    DIAGNOSTIC_HEALTH_STATUS="000", DIAGNOSTIC_MODEL_EXIT="1",
                                    MODEL_LOAD_OUTCOME="failure", HARNESS_OUTCOME="skipped")
            self.assertEqual(result.returncode, 0, result.stderr)
            diagnostic = json.loads((root / "harness/diagnostic.json").read_text())
            self.assertIsNone(diagnostic["model_registered"])
            self.assertIsNone(diagnostic["model_active"])
