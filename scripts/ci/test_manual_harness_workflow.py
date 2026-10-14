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
    def test_source_identity_uses_checkout_not_dispatch_commit(self):
        section = WORKFLOW.read_text().split("- name: Record checked-out source identity\n", 1)[1].split("\n      - ", 1)[0]
        script = "\n".join(line[10:] for line in section.split("run: |\n", 1)[1].splitlines())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.test",
                            "commit", "--allow-empty", "-qm", "fixture"], cwd=root, check=True)
            sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], cwd=root,
                                    env={**os.environ, "GITHUB_ENV": str(root / "env"), "GITHUB_SHA": "0" * 40},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((root / "env").read_text(),
                             f"ASTRA_EXPECTED_BUILD_GIT_SHA={sha}\nASTRA_BUILD_SOURCE_GIT_SHA={sha}\n")

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

    def test_additional_model_configs_are_optional_independent_and_private(self):
        entry = {"name": "secondary-judge", "provider": "openai",
                 "base_url": "https://secondary.example.invalid/v1", "api_key": "secondary-sentinel",
                 "context_window": 64000, "thinking_protocol": "enable_thinking"}
        for extra, accepted in [("", True), ("[]", True), (json.dumps([entry]), True),
                                 ("{}", False), ("[null]", False), ("not-json-sentinel", False),
                                 ('[{"name":"GLM-5.3"}]', False),
                                 ('[{"name":"x"},{"name":"X"}]', False)]:
            with self.subTest(extra=extra), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                result = self.run_block("Validate secrets and prepare private model file", root,
                    GLM_BASE_URL="https://example.invalid/v1", GLM_API_KEY="primary-sentinel",
                    ADDITIONAL_MODELS_JSON=extra)
                self.assertEqual(result.returncode == 0, accepted, result.stderr)
                for secret in ("primary-sentinel", "secondary-sentinel", "not-json-sentinel"):
                    self.assertNotIn(secret, result.stdout + result.stderr)
                if accepted:
                    models = json.loads((root / "harness/models.yaml").read_text())
                    self.assertEqual(len(models), 2 if extra == json.dumps([entry]) else 1)
                    if len(models) == 2:
                        self.assertEqual(models[1], entry)
                else:
                    self.assertFalse((root / "harness/models.yaml").exists())

    def test_summary_excludes_raw_evidence_and_preserves_no_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            summary = root / "job-summary"
            env = {"GITHUB_STEP_SUMMARY": str(summary), "ASTRA_BUILD_SOURCE_GIT_SHA": "a" * 40}
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
                                              "passed": False, "severity": "hard", "score": None,
                                              "detail": "sentinel-secret", "full_detail": "sentinel-secret"}]}]}
            (root / "harness/report.json").write_text(json.dumps(report))
            result = self.run_block("Publish safe result summary", root, **env)
            self.assertEqual(result.returncode, 0, result.stderr)
            safe = (root / "harness/summary.json").read_text()
            self.assertNotIn("sentinel-secret", safe + summary.read_text())
            row = json.loads(safe)["runs"][0]
            self.assertEqual(json.loads(safe)["commit"], "a" * 40)
            self.assertEqual(row["status"], "failed")
            self.assertEqual(row["failure_class"], "InfraProviderError")
            self.assertEqual(row["criteria"], [{"type": "text_contains", "passed": False,
                                                "severity": "hard", "score": None, "evaluation": "reported"}])
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
            (root / "harness/report.preflight.json").write_text(json.dumps({
                "status": "failed", "models": ["glm-5.2", "glm-5.3"],
                "unavailable_model": "glm-5.2", "detail": "sentinel-secret"}))
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
            self.assertEqual(diagnostic["model_lookup_name"], "glm-5.3")
            self.assertEqual(diagnostic["preflight"], {"status": "failed", "models": ["glm-5.2", "glm-5.3"],
                                                     "unavailable_model": "glm-5.2"})
            (root / "harness/model-status.json").unlink()
            result = self.run_block("Diagnose failure without exposing provider responses", root,
                                    GITHUB_STEP_SUMMARY=str(root / "job-summary"),
                                    DIAGNOSTIC_HEALTH_STATUS="000", DIAGNOSTIC_MODEL_EXIT="1",
                                    MODEL_LOAD_OUTCOME="failure", HARNESS_OUTCOME="skipped")
            self.assertEqual(result.returncode, 0, result.stderr)
            diagnostic = json.loads((root / "harness/diagnostic.json").read_text())
            self.assertIsNone(diagnostic["model_registered"])
            self.assertIsNone(diagnostic["model_active"])

    def test_root_and_step_judgment_evidence(self):
        metrics = ("duration_ms", "ttft_ms", "turn_rounds", "tool_calls_count",
                   "prompt_tokens", "cached_input_tokens", "cache_creation_tokens", "completion_tokens")
        outcome = {**dict.fromkeys(metrics, 0), "exit_code": 0, "text": "private-sentinel"}
        def criterion(kind, passed, score, severity):
            return {"criterion": {"type": kind, "question": "private-sentinel"},
                    "passed": passed, "score": score, "severity": severity,
                    "detail": "private-sentinel", "full_detail": "private-sentinel"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            criteria = [criterion("judger", True, None, "quality"),
                        criterion("hard_judger", False, None, "hard"),
                        criterion("judger", True, 1.0, "quality"),
                        criterion("hard_judger", False, 0.0, "hard"),
                        criterion("duration_between", False, None, "soft"),
                        criterion("hard_judger", True, None, "hard")]
            report = {"runs": [{"case_name": "multi_turn", "status": "failed", "run_index": 0,
                                "outcome": outcome, "criteria": [criteria[0], criteria[-1]],
                                "steps": [{"step_index": 0, "passed": False, "criteria": criteria,
                                           "prompt": "private-sentinel", "outcome": outcome},
                                          {"step_index": 1, "passed": False, "prompt": "private-sentinel"}]}]}
            report_file = root / "harness/report.json"
            report_file.write_text(json.dumps(report))
            env = {"GITHUB_STEP_SUMMARY": str(root / "job-summary"), "ASTRA_BUILD_SOURCE_GIT_SHA": "a" * 40}
            result = self.run_block("Publish safe result summary", root, **env)
            self.assertEqual(result.returncode, 0, result.stderr)
            safe = (root / "harness/summary.json").read_text()
            self.assertNotIn("private-sentinel", safe + (root / "job-summary").read_text())
            row = json.loads(safe)["runs"][0]
            self.assertIsNone(row["criteria"][0]["passed"])
            self.assertEqual(row["criteria"][0]["evaluation"], "unscored")
            self.assertTrue(row["criteria"][1]["passed"])
            self.assertEqual(row["criteria"][1]["evaluation"], "reported")
            step = row["steps"][0]
            self.assertFalse(step["passed"])
            self.assertEqual(step["step_index"], 0)
            self.assertFalse(step["criteria"][1]["passed"])
            self.assertEqual(step["criteria"][1]["evaluation"], "reported")
            self.assertTrue(step["criteria"][-1]["passed"])
            self.assertEqual(step["criteria"][2]["score"], 1.0)
            self.assertTrue(step["criteria"][2]["passed"])
            self.assertEqual(step["criteria"][3]["evaluation"], "scored")
            self.assertFalse(step["criteria"][3]["passed"])
            self.assertEqual(step["criteria"][4]["severity"], "soft")
            self.assertEqual(row["steps"][1], {"step_index": 1, "passed": False, "criteria": []})
            # Invalid retained fields must fail before a safe artifact is written.
            for field, invalid in [("score", True), ("score", float("nan")),
                                   ("score", "private-sentinel"), ("passed", "private-sentinel"),
                                   ("severity", "private-sentinel")]:
                with self.subTest(field=field, invalid=invalid):
                    (root / "harness/summary.json").unlink(missing_ok=True)
                    bad = json.loads(json.dumps(report))
                    bad["runs"][0]["criteria"][0][field] = invalid
                    report_file.write_text(json.dumps(bad))
                    result = self.run_block("Publish safe result summary", root, **env)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn("private-sentinel", result.stderr)
                    self.assertFalse((root / "harness/summary.json").exists())
            for field in ("criteria", "steps"):
                for invalid in ({}, "", None):
                    with self.subTest(field=field, collection=invalid):
                        bad = json.loads(json.dumps(report))
                        bad["runs"][0][field] = invalid
                        report_file.write_text(json.dumps(bad))
                        result = self.run_block("Publish safe result summary", root, **env)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertFalse((root / "harness/summary.json").exists())
            bad = json.loads(json.dumps(report))
            bad["runs"][0]["steps"][0]["criteria"] = {}
            report_file.write_text(json.dumps(bad))
            result = self.run_block("Publish safe result summary", root, **env)
            self.assertNotEqual(result.returncode, 0)

    def test_live_command_enables_grading_and_passes_filter_literally(self):
        section = WORKFLOW.read_text().split("- name: Run real harness\n", 1)[1].split("\n      - ", 1)[0]
        script = "\n".join(line[10:] for line in section.split("run: |\n", 1)[1].splitlines())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            subprocess.run(["git", "init", "-q", "-b", "main", str(root)], check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.test",
                            "commit", "--allow-empty", "-qm", "history"], cwd=root, check=True)
            historical_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            subprocess.run(["git", "checkout", "-qb", "candidate"], cwd=root, check=True)
            subprocess.run(["git", "-c", "user.name=Test", "-c", "user.email=test@example.test",
                            "commit", "--allow-empty", "-qm", "candidate"], cwd=root, check=True)
            sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            subprocess.run(["git", "remote", "add", "origin", "https://example.invalid/trusted/astra.git"], cwd=root, check=True)
            subprocess.run(["git", "update-ref", "refs/remotes/origin/main", historical_sha], cwd=root, check=True)
            subprocess.run(["git", "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"], cwd=root, check=True)
            binaries = root / "target/debug"
            binaries.mkdir(parents=True)
            for name in ("astra", "astra-server"):
                (binaries / name).touch()
            stub = binaries / "astra-test"
            stub.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\n"
                            "Path('argv.json').write_text(json.dumps(sys.argv[1:]))\n"
                            "Path(sys.argv[sys.argv.index('--report-file')+1]).write_text('{}')\n")
            stub.chmod(0o700)
            filter_value = "hello* $(touch unexpected)"
            result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], cwd=root,
                                    env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                         "RUNNER_TEMP": str(root), "GITHUB_WORKSPACE": str(root),
                                         "ASTRA_BUILD_SOURCE_GIT_SHA": sha,
                                         "CASE_FILTER": filter_value, "CASE_RUNS": "1"},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            args = json.loads((root / "argv.json").read_text())
            self.assertNotIn("--no-judger", args)
            self.assertEqual(args[args.index("--judger-model") + 1], "glm-5.3")
            self.assertEqual(args[args.index("--filter") + 1], filter_value)
            self.assertFalse((root / "unexpected").exists())
            workspace = root / "harness/workspace"
            self.assertEqual(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=workspace, text=True).strip(), sha)
            subprocess.run(["git", "merge-base", "--is-ancestor", historical_sha, "origin/main"], cwd=workspace, check=True)
            self.assertEqual(subprocess.check_output(["git", "rev-parse", "origin/candidate"], cwd=workspace, text=True).strip(), sha)
            self.assertEqual(subprocess.check_output(["git", "remote", "get-url", "origin"], cwd=workspace, text=True).strip(),
                             "https://example.invalid/trusted/astra.git")
            subprocess.run(["git", "update-ref", "refs/remotes/origin/main", sha], cwd=workspace, check=True)
            self.assertEqual(subprocess.check_output(["git", "rev-parse", "origin/main"], cwd=root, text=True).strip(), historical_sha)
            self.assertFalse((workspace / "target").exists())
