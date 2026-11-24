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
            text=True, env={**os.environ, "RUNNER_TEMP": str(root),
                            "HARNESS_MODEL": "kimi-k3.1-pro", **env}, timeout=10,
        )

    def test_kimi_is_required_and_seeded_privately(self):
        kimi = {"name": "kimi-k3.1-pro", "provider": "openai",
                "base_url": "https://example.invalid/v1", "api_key": "kimi-sentinel"}
        glm = {**kimi, "name": "glm-5.3", "api_key": "glm-sentinel"}
        for profile, entries, expected in (
            ("regression", [kimi], [kimi]),
            ("regression", [glm, kimi], [kimi]),
            ("full", [kimi, glm], [kimi, glm]),
            ("regression", [glm], None),
            ("full", [{**kimi, "name": "KIMI-K3.1-PRO"}], None),
            ("full", [kimi, {**kimi, "name": "KIMI-K3.1-PRO"}], None),
            ("full", [None], None),
        ):
            with self.subTest(profile=profile, configured=len(entries)), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                result = self.run_block(
                    "Validate secrets and prepare private model file", root,
                    CASE_PROFILE=profile, GLM_BASE_URL="", GLM_API_KEY="", ADDITIONAL_MODELS_JSON=json.dumps(entries),
                )
                path = root / "harness/models.yaml"
                for secret in ("kimi-sentinel", "glm-sentinel"):
                    self.assertNotIn(secret, result.stdout + result.stderr)
                if expected is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(path.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                    self.assertEqual(json.loads(path.read_text()), expected)

    def test_kimi_reuses_provider_credentials_when_no_explicit_entry_exists(self):
        kimi = {"name": "kimi-k3.1-pro", "api_key": "explicit-sentinel"}
        glm = {"name": "glm-5.3", "api_key": "glm-sentinel"}
        for url, key, entries, expected in (
            ("https://example.invalid/v1", "provider-sentinel", [glm], True),
            ("", "", [kimi, glm], True),
            ("", "provider-sentinel", [glm], False),
            ("http://example.invalid/v1", "provider-sentinel", [glm], False),
            ("https://user:provider-sentinel@example.invalid/v1", "provider-sentinel", [glm], False),
        ):
            with self.subTest(configured=expected), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                result = self.run_block(
                    "Validate secrets and prepare private model file", root,
                    CASE_PROFILE="regression", GLM_BASE_URL=url, GLM_API_KEY=key,
                    ADDITIONAL_MODELS_JSON=json.dumps(entries),
                )
                path = root / "harness/models.yaml"
                for secret in ("provider-sentinel", "explicit-sentinel", "glm-sentinel"):
                    self.assertNotIn(secret, result.stdout + result.stderr)
                if expected:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    selected = json.loads(path.read_text())
                    self.assertEqual(len(selected), 1)
                    self.assertEqual(selected[0]["name"], "kimi-k3.1-pro")
                    self.assertEqual(selected[0]["api_key"], kimi["api_key"] if kimi in entries else key)
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(path.exists())

    def test_seed_failure_reports_only_stage_and_removes_model_credentials(self):
        section = WORKFLOW.read_text().split("- name: Register model through the normal admin flow\n", 1)[1]
        block = section.split("run: |\n", 1)[1].split("\n      - ", 1)[0]
        script = "\n".join(line[10:] for line in block.splitlines())
        for failing_command in ("register", "login", "model", ""):
            with self.subTest(command=failing_command), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                private = root / "harness"
                private.mkdir()
                model_file = private / "models.yaml"
                model_file.write_text("secret-sentinel")
                classifier = root / "scripts/ci/harness_seed_failure.py"
                classifier.parent.mkdir(parents=True)
                shutil.copy(WORKFLOW.parents[2] / "scripts/ci/harness_seed_failure.py", classifier)
                cli = root / "target/debug/astra"
                cli.parent.mkdir(parents=True)
                cli.write_text('#!/bin/bash\nif [ "$2" = "$FAIL_COMMAND" ]; then echo private-sentinel; exit 3; fi\n')
                cli.chmod(0o700)
                result = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script], cwd=root,
                    env={**os.environ, "RUNNER_TEMP": str(root),
                         "GITHUB_OUTPUT": str(root / "outputs"), "HARNESS_MODEL": "kimi-k3.1-pro", "FAIL_COMMAND": failing_command},
                    capture_output=True, text=True, timeout=10,
                )
                self.assertFalse(model_file.exists())
                self.assertNotIn("sentinel", result.stdout + result.stderr)
                if failing_command:
                    stage = "model-load" if failing_command == "model" else f"admin-{failing_command}"
                    self.assertEqual(result.returncode, 3)
                    expected = f"failure_stage={stage}\nexit_code=3\n"
                    if failing_command == "model":
                        expected += "error_code=unknown\n"
                    self.assertEqual((root / "outputs").read_text(), expected)
                    self.assertIn("private-sentinel", (private / f"{stage}.log").read_text())
                else:
                    self.assertEqual(result.returncode, 0)
                    self.assertFalse((root / "outputs").exists())

    def test_seed_retry_only_for_structured_transient_probe_errors(self):
        from harness_seed_failure import classify, RETRYABLE
        for code, status in (("model_probe_timeout", "504 Gateway Timeout"),
                             ("model_probe_network", "502 Bad Gateway"),
                             ("model_probe_rate_limited", "429 Too Many Requests"),
                             ("model_probe_credentials_or_access", "400 Bad Request"),
                             ("model_probe_configuration", "400 Bad Request")):
            body = json.dumps({"error_code": code, "detail": "secret"})
            for line in (
                f'  model check failed for kimi-k3.1-pro: HTTP {status}: ' + body,
                f'  could not set up or confirm model kimi-k3.1-pro: request failed ({status[:3]}): ' + body,
            ):
                self.assertEqual(classify(line, "kimi-k3.1-pro"), code)
            self.assertEqual(code in RETRYABLE, code in ("model_probe_timeout", "model_probe_network", "model_probe_rate_limited"))
        self.assertEqual(classify('secret model_probe_timeout', 'kimi-k3.1-pro'), 'unknown')
        self.assertEqual(classify('  model check failed for other: HTTP 504 Gateway Timeout: {"error_code":"model_probe_timeout"}', 'kimi-k3.1-pro'), 'unknown')
        self.assertEqual(classify('  could not set up or confirm model kimi-k3.1-pro: request failed (504): {"error_code":[]}', 'kimi-k3.1-pro'), 'unknown')

    def test_seed_retries_a_transient_model_probe_once(self):
        section = WORKFLOW.read_text().split("- name: Register model through the normal admin flow\n", 1)[1]
        block = section.split("run: |\n", 1)[1].split("\n      - ", 1)[0]
        script = "\n".join(line[10:] for line in block.splitlines())
        errors = (
            '  could not set up or confirm model kimi-k3.1-pro: request failed (504): {"error_code":"model_probe_timeout","detail":"secret-sentinel"}',
            '  model check failed for kimi-k3.1-pro: HTTP 504 Gateway Timeout: {"error_code":"model_probe_timeout","detail":"secret-sentinel"}',
        )
        for error in errors:
            with self.subTest(error=error), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                private = root / "harness"
                private.mkdir()
                (private / "models.yaml").write_text("secret-sentinel")
                classifier = root / "scripts/ci/harness_seed_failure.py"
                classifier.parent.mkdir(parents=True)
                shutil.copy(WORKFLOW.parents[2] / "scripts/ci/harness_seed_failure.py", classifier)
                cli = root / "target/debug/astra"
                cli.parent.mkdir(parents=True)
                cli.write_text(
                    '#!/bin/bash\n'
                    'if [ "$2" = model ] && [ ! -e "$RUNNER_TEMP/attempted" ]; then\n'
                    '    touch "$RUNNER_TEMP/attempted"\n'
                    '    echo "$PROBE_FAILURE" >&2\n'
                    '    exit 3\n'
                    'fi\n'
                )
                cli.chmod(0o700)
                result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], cwd=root,
                    env={**os.environ, "RUNNER_TEMP": str(root), "HARNESS_MODEL": "kimi-k3.1-pro",
                         "GITHUB_OUTPUT": str(root / "outputs"), "PROBE_FAILURE": error},
                    capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("retrying once", result.stdout)
                self.assertNotIn("secret-sentinel", result.stdout + result.stderr)
                self.assertFalse((private / "models.yaml").exists())

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
                                    MODEL_LOAD_OUTCOME="failure", HARNESS_OUTCOME="skipped",
                                    MODEL_LOAD_FAILURE_STAGE="model-load", MODEL_LOAD_EXIT_CODE="3")
            self.assertEqual(result.returncode, 0, result.stderr)
            text = (root / "harness/diagnostic.json").read_text()
            self.assertNotIn("sentinel-secret", text + (root / "job-summary").read_text())
            diagnostic = json.loads(text)
            self.assertTrue(diagnostic["model_registered"])
            self.assertFalse(diagnostic["model_active"])
            self.assertEqual(diagnostic["harness_step"], "skipped")
            self.assertEqual(diagnostic["model_load_failure"], {"stage": "model-load", "exit_code": 3})
            self.assertEqual(diagnostic["model_lookup_name"], "kimi-k3.1-pro")
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
                "error_kind": "api_error", "error_code": "execution_binding_provider_required",
                "final_state": None, "interruption_kind": None})
            self.assertNotIn("sentinel-secret", (root / "harness/diagnostic.json").read_text()
                             + (root / "job-summary").read_text())

    def test_preflight_terminal_categories_use_the_existing_closed_projection(self):
        allowed = ("budget_exhausted", "empty_completion", "token_budget_exceeded",
                   "rate_limited", "cooldown_rejected", "user_cancelled", "context_overflow",
                   "auth_failure", "critical_verdict", "approval_rejected", "server_overload",
                   "stream_transport", "stream_idle", "provider_deadline", "harness_blocked",
                   "harness_paused", "interrupted", "execution_incomplete", "executor_dropped")
        cases = [("execution", "interrupted", kind, kind) for kind in allowed]
        cases += [("execution", "interrupted", kind, None) for kind in
                  (None, [], {}, 1, "private_sentinel", "x" * 256)]
        cases += [(stage, state, "provider_deadline", None) for stage, state in
                  (("terminal_protocol", "interrupted"), ("probe_process", "interrupted"),
                   ("session_cleanup", "interrupted"), ("execution", "completed"),
                   ("execution", "failed"), ("execution", "private_sentinel"),
                   ("execution", None), ("execution", {}))]
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "harness").mkdir()
            for stage, state, kind, expected in cases:
                with self.subTest(stage=stage, state=state, kind=kind):
                    (root / "harness/report.preflight.json").write_text(json.dumps({
                        "status": "failed", "models": ["fixture-model"], "unavailable_model": None,
                        "failure": {"model": "fixture-model", "stage": stage, "exit_code": 5,
                                    "error_kind": "partial", "error_code": None,
                                    "final_state": state, "interruption_kind": kind,
                                    "text": "private_sentinel", "detail": "private_sentinel"}}))
                    result = self.run_block("Diagnose failure without exposing provider responses", root,
                        GITHUB_STEP_SUMMARY=str(root / "job-summary"), DIAGNOSTIC_HEALTH_STATUS="200",
                        DIAGNOSTIC_MODEL_EXIT="0", MODEL_LOAD_OUTCOME="success", HARNESS_OUTCOME="failure")
                    self.assertEqual(result.returncode, 0, result.stderr)
                    raw = (root / "harness/diagnostic.json").read_text()
                    failure = json.loads(raw)["preflight"]["failure"]
                    self.assertEqual(failure["exit_code"], 5)
                    self.assertEqual(failure["interruption_kind"], expected)
                    expected_state = state if (stage == "execution" and isinstance(state, str)
                        and state in ("completed", "interrupted", "failed")) else None
                    self.assertEqual(failure["final_state"], expected_state)
                    self.assertNotIn("private_sentinel", raw + result.stdout + result.stderr
                                     + (root / "job-summary").read_text())

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_live_command_enables_grading_and_passes_filter_literally(self):
        self.run_live_command("full")

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_regression_runs_reviewed_cases_without_disabling_grading(self):
        self.run_live_command("regression")

    def test_edge_shell_contract_runs_in_harness_job_even_if_live_cases_fail(self):
        workflow = WORKFLOW.read_text()
        step = workflow.split("- name: Harness regression - Edge WS shell contract\n", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("!cancelled() && steps.seed.outcome == 'success'", step)
        self.assertIn("scripts/ci/client_compatibility.py", step)
        self.assertIn("--edge-shell-only", step)
        self.assertIn("--existing-admin", step)
        self.assertIn("-p astra-edge --bin astra-edge", workflow)
        self.assertIn("/harness/edge-shell.json", workflow)

    def run_live_command(self, profile):
        workflow = WORKFLOW.read_text()
        self.assertNotIn("matrix:", workflow)
        model = re.search(r"^      HARNESS_MODEL: (\S+)$", workflow, re.M)[1]
        self.assertEqual(model, "kimi-k3.1-pro")
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
            for name in ("astra", "astra-edge", "astra-server"):
                (binaries / name).touch()
            stub = binaries / "astra-test"
            stub.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\n"
                            "if '--help' in sys.argv: print('--progress-file'); sys.exit(0)\n"
                            "Path('argv.json').write_text(json.dumps(sys.argv[1:]))\n"
                            "Path(sys.argv[sys.argv.index('--report-file')+1]).write_text('{}')\n"
                            "import os\n"
                            "e={'schema':'astra.harness_progress.v8','type':'suite_started',"
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
                                    env={**os.environ, "HARNESS_MODEL": model, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                         "RUNNER_TEMP": str(root), "GITHUB_WORKSPACE": str(root),
                                         "ASTRA_BUILD_SOURCE_GIT_SHA": sha,
                                         "CASE_FILTER": filter_value, "CASE_RUNS": "1", "CASE_PROFILE": profile},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 1, result.stderr)  # no terminal is never a pass
            args = json.loads((root / "argv.json").read_text())
            self.assertNotIn("--no-judger", args)
            self.assertEqual(args[args.index("--judger-model") + 1], model)
            self.assertEqual(args[args.index("--force-model") + 1], model)
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
