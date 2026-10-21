"""Exercise the workflow's actual Python blocks without provider calls or builds."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest


WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/harness-manual.yml"


def python_block(step):
    section = WORKFLOW.read_text().split(f"- name: {step}\n", 1)[1]
    block = re.search(r"python3 - <<'PY'\n(.*?)\n          PY", section, re.S)[1]
    return "\n".join(line[10:] for line in block.splitlines())


class ManualHarnessWorkflowTests(unittest.TestCase):
    def test_automatic_lane_reuses_execution_and_keeps_trust_boundary(self):
        workflow = (WORKFLOW.parent / "harness-regression.yml").read_text()
        self.assertIn("\n  push:\n    branches: [main]", workflow)
        self.assertIn("\n  pull_request_target:\n    branches: [main]", workflow)
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", workflow)
        self.assertIn("github.actor != 'dependabot[bot]'", workflow)
        self.assertIn("github.event.pull_request.user.login != 'dependabot[bot]'", workflow)
        self.assertNotIn("\n  pull_request:\n", workflow)
        self.assertIn("uses: ./.github/workflows/harness-manual.yml", workflow)
        self.assertIn("ref: ${{ github.event.pull_request.head.sha || github.sha }}", workflow)
        self.assertIn("repository: ${{ github.event.pull_request.head.repo.full_name || github.repository }}", workflow)
        self.assertIn("profile: regression", workflow)
        self.assertNotIn("secrets: inherit", workflow)

    def test_actual_trust_expression_rejects_name_or_id_spoofing(self):
        workflow = (WORKFLOW.parent / "harness-regression.yml").read_text()
        # Evaluate the actual workflow's Boolean/equality-only subset, not a
        # second policy function. GitHub also compares these strings ignoring
        # case; every canonical fixture below uses GitHub's actual casing.
        expression = re.search(r"if: >-\n\s*\$\{\{(.*?)\}\}", workflow, re.S)[1]
        expression = " ".join(expression.replace("&&", "and").replace("||", "or").split())
        trusted = {"XuPeng-SH/Astra": 1355430593, "gouhongshen/Astra": 1355443432,
                   "loveRhythm1990/Astra": 1355489820}

        def allowed(name, repo_id, *, event="pull_request_target", actor="contributor", author="contributor",
                    destination="matrixorigin/Astra"):
            github = SimpleNamespace(repository=destination, event_name=event, actor=actor,
                event=SimpleNamespace(repository=SimpleNamespace(id=100),
                    pull_request=SimpleNamespace(user=SimpleNamespace(login=author), head=SimpleNamespace(
                        repo=SimpleNamespace(full_name=name, id=repo_id)))))
            return eval(expression, {"__builtins__": {}}, {"github": github})

        for name, repo_id in trusted.items():
            with self.subTest(name=name):
                self.assertTrue(allowed(name, repo_id))
                self.assertFalse(allowed(name, repo_id + 1))
                self.assertFalse(allowed("attacker/Astra", repo_id))
                self.assertFalse(allowed(name, repo_id, actor="dependabot[bot]"))
                self.assertFalse(allowed(name, repo_id, actor="maintainer", author="dependabot[bot]"))
                self.assertFalse(allowed(name, repo_id, destination="attacker/Astra"))
        self.assertFalse(allowed("attacker/Astra", 999))
        self.assertFalse(allowed("", 0))  # Deleted/absent head repository.
        self.assertTrue(allowed("matrixorigin/Astra", 100))
        self.assertFalse(allowed("matrixorigin/Astra", 101))
        self.assertFalse(allowed("matrixorigin/Astra", 100, actor="maintainer", author="dependabot[bot]"))
        self.assertTrue(allowed("", 0, event="push", actor="dependabot[bot]"))
        self.assertIn("allow-unsafe-pr-checkout: ${{ github.event_name == 'pull_request_target' }}",
                      WORKFLOW.read_text())

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
            self.assertEqual(len(json.loads(path.read_text())), 1)
            flash = model
            self.assertEqual(flash["name"], "deepseek-v4.1-flash")
            self.assertEqual(flash["api_key"], "sentinel-secret")
            self.assertEqual(flash["base_url"], "https://example.invalid/v1")
            self.assertEqual(flash["thinking_protocol"], "reasoning_effort")
            self.assertEqual(flash["prompt_cache_capability"], {
                "schema_version": 2, "protocol": "openai_auto_prefix",
                "volatile_placement": "append_only_user_tail", "volatile_delivery": "required_only",
                "reuse_scope": "conversation_turns"})
            self.assertNotIn("sentinel-secret", result.stdout + result.stderr)

    def test_additional_model_configs_are_optional_independent_and_private(self):
        entry = {"name": "secondary-judge", "provider": "openai",
                 "base_url": "https://secondary.example.invalid/v1", "api_key": "secondary-sentinel",
                 "context_window": 64000, "thinking_protocol": "enable_thinking"}
        explicit_flash = {**entry, "name": "deepseek-v4.1-flash"}
        for extra, accepted in [("", True), ("[]", True), (json.dumps([entry]), True),
                                 (json.dumps([explicit_flash]), True),
                                 ("{}", False), ("[null]", False), ("not-json-sentinel", False),
                                 (json.dumps([{**entry, "name": "glm-5.3"}]), True),
                                 ('[{"name":"deepseek-v4.1-flash"},{"name":"DEEPSEEK-V4.1-FLASH"}]', False),
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
                    self.assertEqual(len(models), 1 if extra in ("", "[]", json.dumps([explicit_flash])) else 2)
                    if extra == json.dumps([entry]):
                        self.assertEqual(models[0], entry)
                    if extra == json.dumps([explicit_flash]):
                        self.assertEqual(models[0], explicit_flash)
                else:
                    self.assertFalse((root / "harness/models.yaml").exists())

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
            self.assertEqual(diagnostic["model_lookup_name"], "deepseek-v4.1-flash")
            self.assertEqual(diagnostic["preflight"], {"status": "failed", "models": ["glm-5.2", "glm-5.3"],
                                                     "unavailable_model": "glm-5.2"})
            (root / "harness/report.preflight.json").write_text(json.dumps({
                "status": "failed", "models": ["glm-5.3"], "unavailable_model": None,
                "failure": {"model": "glm-5.3", "stage": "execution", "exit_code": 3,
                            "error_kind": "api_error", "error_code": "execution_binding_provider_required",
                            "detail": "sentinel-secret"}}))
            (root / "harness/model-status.json").unlink()
            result = self.run_block("Diagnose failure without exposing provider responses", root,
                                    GITHUB_STEP_SUMMARY=str(root / "job-summary"),
                                    DIAGNOSTIC_HEALTH_STATUS="000", DIAGNOSTIC_MODEL_EXIT="1",
                                    MODEL_LOAD_OUTCOME="failure", HARNESS_OUTCOME="skipped")
            self.assertEqual(result.returncode, 0, result.stderr)
            diagnostic = json.loads((root / "harness/diagnostic.json").read_text())
            self.assertIsNone(diagnostic["model_registered"])
            self.assertIsNone(diagnostic["model_active"])
            self.assertEqual(diagnostic["preflight"]["failure"], {
                "model": "glm-5.3", "stage": "execution", "exit_code": 3,
                "error_kind": "api_error", "error_code": "execution_binding_provider_required"})
            self.assertNotIn("sentinel-secret", (root / "harness/diagnostic.json").read_text()
                             + (root / "job-summary").read_text())

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_live_command_enables_grading_and_passes_filter_literally(self):
        self.run_live_command("full")

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_regression_runs_reviewed_cases_without_disabling_grading(self):
        self.run_live_command("regression")

    def run_live_command(self, profile):
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
                            "if '--help' in sys.argv: print('--progress-file'); sys.exit(0)\n"
                            "Path('argv.json').write_text(json.dumps(sys.argv[1:]))\n"
                            "Path(sys.argv[sys.argv.index('--report-file')+1]).write_text('{}')\n"
                            "import os\n"
                            "e={'schema':'astra.harness_progress.v5','type':'suite_started',"
                            "'run_id':'cli','sequence':1,'total':1,'commit':os.environ['ASTRA_BUILD_SOURCE_GIT_SHA']}\n"
                            "Path(sys.argv[sys.argv.index('--progress-file')+1]).write_text(json.dumps(e)+'\\n')\n")
            stub.chmod(0o700)
            helpers = root / "scripts/ci"
            helpers.mkdir(parents=True)
            shutil.copyfile(WORKFLOW.parents[2] / "scripts/ci/manual_harness.py", helpers / "manual_harness.py")
            shutil.copyfile(WORKFLOW.parents[2] / "scripts/ci/select_harness_cases.py", helpers / "select_harness_cases.py")
            harness = root / "crates/astra-test-harness"
            harness.mkdir(parents=True)
            shutil.copyfile(WORKFLOW.parents[2] / "crates/astra-test-harness/ci-regression.txt", harness / "ci-regression.txt")
            shutil.copytree(WORKFLOW.parents[2] / "crates/astra-test-harness/cases", harness / "cases")
            supervisor = root / "scripts/harness"
            supervisor.mkdir()
            shutil.copyfile(WORKFLOW.parents[2] / "scripts/harness/process_supervisor.py", supervisor / "process_supervisor.py")
            filter_value = "hello* $(touch unexpected)"
            result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], cwd=root,
                                    env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                         "RUNNER_TEMP": str(root), "GITHUB_WORKSPACE": str(root),
                                         "ASTRA_BUILD_SOURCE_GIT_SHA": sha,
                                         "CASE_FILTER": filter_value, "CASE_RUNS": "1", "CASE_PROFILE": profile},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 1, result.stderr)  # no terminal is never a pass
            args = json.loads((root / "argv.json").read_text())
            self.assertNotIn("--no-judger", args)
            self.assertEqual(args[args.index("--judger-model") + 1], "deepseek-v4.1-flash")
            self.assertEqual(args[args.index("--force-model") + 1], "deepseek-v4.1-flash")
            self.assertEqual(args[args.index("--filter") + 1], "*" if profile == "regression" else filter_value)
            suite = Path(args[args.index("--suite") + 1])
            if profile == "regression":
                selected = [line for line in (harness / "ci-regression.txt").read_text().splitlines()
                            if line and not line.startswith("#")]
                self.assertEqual(sorted(p.name for p in suite.iterdir()), sorted(Path(p).name for p in selected))
                for path in selected:
                    self.assertEqual((suite / Path(path).name).read_bytes(), (harness / "cases" / path).read_bytes())
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
