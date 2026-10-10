#!/usr/bin/env python3
"""Exercise CI gates and the real Makefile's online shard dispatch offline."""

import json
from itertools import product
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from test_release_build_shells import workflow_run_script


ROOT = Path(__file__).resolve().parents[2]
SHARDS = {
    "core-runtime": "astra-runtime",
    "core-turn-core": "astra-turn-core",
    "core-services": "astra-services",
}


class CliSharedTests(unittest.TestCase):
    def setUp(self):
        self.workflow = (ROOT / ".github/workflows/test.yml").read_text()
        self.producer = self.workflow.split("\n  cli-test-build:\n", 1)[1].split("\n  shard-a:\n", 1)[0]
        self.consumer = self.workflow.split("\n  shard-a:\n", 1)[1].split("\n  shard-b:\n", 1)[0]

    def test_required_labels_share_the_complete_inventory(self):
        matrix = self.consumer.split("      matrix:\n", 1)[1].split("    steps:\n", 1)[0]
        labels = re.search(r"segment: \[([^\]]+)\]", matrix).group(1).split(", ")
        self.assertEqual(len(labels), 5)
        self.assertEqual(set(labels), {
            "non-edge", "edge-shell", "edge-fs-tools", "edge-git-gix", "edge-rest",
        })
        self.assertIn('name: "Test: astra-cli (${{ matrix.segment }})"', self.consumer)
        command = re.search(r"- name: Test complete CLI inventory\n\s+run: ([^\n]+)", self.producer).group(1)
        self.assertEqual(command,
                         "cargo nextest run --locked -p astra-cli --lib --bins --profile ci")

    def test_required_labels_reject_failed_cancelled_or_missing_test_result(self):
        gate = self.consumer.split("      - name: Require complete CLI test result\n", 1)[1].split("      - ", 1)[0]
        self.assertIn("if: env.RUN_TESTS == 'true'", gate)
        self.assertIn("TEST_RESULT: ${{ needs.cli-test-build.result }}", gate)
        script = gate.split("        run: ", 1)[1].strip()
        for status in ("success", "failure", "cancelled", "skipped", ""):
            with self.subTest(status=status):
                result = subprocess.run(["bash", "-c", script],
                    env={**os.environ, "TEST_RESULT": status}, capture_output=True)
                self.assertEqual(result.returncode == 0, status == "success")
        self.assertIn("needs: [scope, cli-test-build]", self.consumer)
        self.assertIn("if: ${{ !cancelled() }}", self.consumer)
        for section in (self.producer, self.consumer):
            self.assertIn("needs.scope.result != 'success'", section)
            self.assertIn("needs.scope.outputs.test_cli == 'true'", section)
        for job in ("shard-b", "shard-c", "shard-d"):
            header = self.workflow.split(f"\n  {job}:\n", 1)[1].split("    steps:\n", 1)[0]
            self.assertIn("needs: scope", header)
            self.assertNotIn("cli-test-build", header)

    def test_status_labels_do_not_repeat_setup_builds_or_artifact_transfers(self):
        self.assertNotIn("uses:", self.consumer)
        self.assertNotRegex(self.consumer, r"\bcargo(?:-nextest)?\b")
        self.assertNotIn("continue-on-error", self.consumer)
        self.assertNotIn("archive", self.producer)
        timings = self.producer.split("      - name: Retain CLI test timings\n", 1)[1]
        self.assertIn("if: ${{ !cancelled() }}", timings)
        self.assertIn("path: target/nextest/ci/junit.xml", timings)
        self.assertEqual(self.producer.count("actions/upload-artifact@"), 1)
        self.assertIn("save-cache: ${{ github.event_name == 'push' }}", self.producer)
        self.assertIn("cancel-in-progress: true", self.workflow)


class ParallelGateTests(unittest.TestCase):
    def test_shard_gates_reject_failure_cancellation_and_unexpected_skip(self):
        for step, variables in (
            ("Require terminal PTY shards", ("SHARDS_RESULT",)),
            ("Require macOS coordination and CLI execution", ("COORDINATION_RESULT", "TERMINAL_RESULT")),
        ):
            script = workflow_run_script(".github/workflows/test.yml", step)
            for required in (True, False):
                for statuses in product(("success", "failure", "cancelled", "skipped", ""), repeat=len(variables)):
                    with self.subTest(step=step, required=required, statuses=statuses):
                        result = subprocess.run(["bash", "-c", script], env={
                            **os.environ, "SHARDS_REQUIRED": str(required).lower(),
                            **dict(zip(variables, statuses)),
                        }, capture_output=True, text=True)
                        expected = all(status == "success" or (not required and status == "skipped") for status in statuses)
                        self.assertEqual(result.returncode == 0, expected)

    def test_online_gate_requires_all_matrix_jobs_to_succeed(self):
        script = workflow_run_script(".github/workflows/test.yml", "Require online shards")
        for status in ("success", "failure", "cancelled", "skipped", ""):
            for moi_status in ("success", "failure", "cancelled", "skipped", ""):
                with self.subTest(status=status, moi_status=moi_status):
                    result = subprocess.run(["bash", "-c", script], env={
                        **os.environ, "SHARDS_RESULT": status, "MOI_RESULT": moi_status,
                    }, capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, status == moi_status == "success")

    def test_parallel_jobs_depend_only_on_scope_and_gates_depend_on_shards(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        for job in ("terminal-pty-shards", "test-online"):
            header = workflow.split(f"\n  {job}:\n", 1)[1].split("    steps:\n", 1)[0]
            self.assertIn("    needs: scope\n", header)
            self.assertIn("!cancelled()", header)
            self.assertIn("fail-fast: false", header)
        terminal = workflow.split("\n  terminal-pty:\n", 1)[1].split("\n  terminal-pty-shards:", 1)[0]
        self.assertIn("needs: [scope, terminal-pty-shards]", terminal)
        self.assertIn("needs.scope.result != 'success'", terminal)
        self.assertIn("needs.scope.outputs.test_cli == 'true'", terminal)
        self.assertIn("needs.scope.outputs.test_core == 'true'", terminal)
        self.assertIn("needs.terminal-pty-shards.result", terminal)
        self.assertIn("if: ${{ !cancelled() }}", terminal)
        coordination = workflow.split("\n  macos-workspace-coordination:\n", 1)[1].split("\n  macos-workspace-coordination-tests:", 1)[0]
        self.assertIn('name: "Test: macOS workspace coordination"', coordination)
        self.assertIn("needs: [scope, macos-workspace-coordination-tests, terminal-pty-shards]", coordination)
        self.assertIn("needs.scope.result != 'success'", coordination)
        self.assertIn("if: ${{ !cancelled() }}", coordination)
        contracts = workflow.split("\n  macos-workspace-coordination-tests:\n", 1)[1].split("    steps:\n", 1)[0]
        self.assertIn("needs: scope", contracts)
        self.assertNotIn("terminal-pty-shards", contracts, "Tools contracts must not wait for the CLI build")
        online = workflow.split("\n  test-online:\n", 1)[1]
        self.assertIn("lane: [core-runtime, core-turn-core, core-services, integration]", online)
        self.assertIn("matrix.lane != 'integration' && needs.scope.outputs.online_core == 'true'", online)
        self.assertIn("matrix.lane == 'integration' && needs.scope.outputs.online_integration == 'true'", online)
        self.assertIn('ASTRA_ONLINE_LANE: ${{ matrix.lane }}', online)
        self.assertIn('ASTRA_TEST_DB_IT_TEST_THREADS: "1"', online)
        self.assertIn("run: make dev-deps-up", online)
        core = workflow.split("\n  test-online-core:\n", 1)[1].split("\n  test-online:", 1)[0]
        self.assertIn('name: "Test: online (core)"', core)
        self.assertIn("needs: [test-online, moi-compatibility]", core)
        self.assertIn("needs.test-online.result", core)
        self.assertIn("needs.moi-compatibility.result", core)
        self.assertIn("if: ${{ !cancelled() }}", core)

    def test_moi_consumer_gate_always_executes_the_frozen_client(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        job = workflow.split("\n  moi-compatibility:\n", 1)[1].split("\n  test-online:", 1)[0]
        self.assertNotIn("needs: scope", job)
        self.assertNotIn("RUN_TESTS", job)
        self.assertIn("--features moi-compat-tests", job)
        self.assertIn("--test moi_fresh_astra -- --ignored --nocapture", job)
        self.assertNotIn("secrets.", job)


class OnlineShardTests(unittest.TestCase):
    def run_lane(self, lane, fail_package="", sdk=False):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            (fixture / "Makefile").write_text((ROOT / "Makefile").read_text())
            (fixture / ".env").touch()
            fake_bin = fixture / "bin"
            fake_bin.mkdir()
            mysql = fixture / "scripts/dev/mysql-client.sh"
            mysql.parent.mkdir(parents=True)
            recorder = f"#!{sys.executable}\n" + '''
import json, os, sys
from pathlib import Path
kind = Path(sys.argv[0]).name
args = sys.argv[1:]
record = {"kind": kind, "args": args,
          "database": os.environ.get("ASTRA_DATABASE"),
          "test_database": os.environ.get("ASTRA_TEST_DATABASE"),
          "db_enabled": os.environ.get("ASTRA_TEST_DB_IT")}
with open(os.environ["ASTRA_CI_CALLS"], "a") as output:
    output.write(json.dumps(record) + "\\n")
sys.exit(1 if kind == "cargo" and os.environ["ASTRA_CI_FAIL_PACKAGE"] in args else 0)
'''
            for path in (mysql, fake_bin / "cargo", fake_bin / "npm"):
                path.write_text(recorder)
                path.chmod(0o755)
            calls = fixture / "calls.jsonl"
            environment = {
                key: value for key, value in os.environ.items()
                if not key.startswith(("ASTRA_", "MATRIXONE_", "MEMORIA_", "CARGO_", "RUST_", "MAKE"))
            }
            environment.update({
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_CI_CALLS": str(calls), "ASTRA_CI_FAIL_PACKAGE": fail_package,
                "ASTRA_TEST_DATABASE": "astra_fixture", "ASTRA_TEST_DB_IT_TEST_THREADS": "1",
                "ASTRA_SDK_ONLINE_E2E": "1" if sdk else "0", "ASTRA_MEMORIA_ONLINE": "0",
            })
            if lane is not None:
                environment["ASTRA_ONLINE_LANE"] = lane
            result = subprocess.run(
                ["make", "test-online", "NEXTEST_ONLINE_PROFILE=strict-online-ci"],
                cwd=fixture, env=environment, capture_output=True, text=True, timeout=20,
            )
            records = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            return result, records

    def cargo_calls(self, lane, **kwargs):
        result, records = self.run_lane(lane, **kwargs)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any(row["kind"] == "npm" for row in records))
        return [row for row in records if row["kind"] == "cargo"]

    def test_shards_cover_combined_core_with_identical_test_selections(self):
        combined = self.cargo_calls("core")
        self.assertEqual(len(combined), 3)
        split = []
        for lane, package in SHARDS.items():
            with self.subTest(lane=lane):
                calls = self.cargo_calls(lane, sdk=True)
                self.assertEqual(len(calls), 1)
                call = calls[0]
                self.assertEqual(call["args"][call["args"].index("-p") + 1], package)
                self.assertEqual(call["database"], "astra_fixture_" + lane.replace("-", "_"))
                self.assertEqual(call["db_enabled"], "1")
                self.assertEqual(call["args"][call["args"].index("-j") + 1], "1")
                self.assertEqual(call["args"][call["args"].index("--profile") + 1], "strict-online-ci")
                if package == "astra-services":
                    self.assertEqual(call["test_database"], call["database"])
                split.append(call)
        self.assertEqual([row["args"] for row in split], [row["args"] for row in combined])
        self.assertEqual(len({row["database"] for row in split}), 3)

    def test_default_all_still_includes_core_and_integration(self):
        expected = self.cargo_calls("core") + self.cargo_calls("integration")
        for lane in (None, "all"):
            with self.subTest(lane=lane):
                self.assertEqual(self.cargo_calls(lane), expected)

    def test_every_shard_failure_propagates(self):
        for lane, package in {**SHARDS, "integration": "astra-plan"}.items():
            with self.subTest(lane=lane):
                result, records = self.run_lane(lane, fail_package=package)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("failed suites:", result.stdout)
                self.assertTrue(any(package in row["args"] for row in records))

    def test_combined_core_keeps_running_remaining_suites_after_failure(self):
        result, records = self.run_lane("core", fail_package="astra-runtime")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len([row for row in records if row["kind"] == "cargo"]), 3)

    def test_each_shard_resets_only_its_own_database(self):
        for lane in (*SHARDS, "integration"):
            with self.subTest(lane=lane):
                result, records = self.run_lane(lane)
                self.assertEqual(result.returncode, 0, result.stderr)
                database = "astra_fixture_" + lane.replace("-", "_")
                ddl = [row for row in records if row["kind"] == "mysql-client.sh"]
                self.assertEqual(len(ddl), 1)
                self.assertEqual(ddl[0]["args"], [
                    "-e", f"DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database};",
                ])

    def test_unknown_lane_fails_before_database_reset_or_test_execution(self):
        result, records = self.run_lane("core-misspelled")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid ASTRA_ONLINE_LANE", result.stdout)
        self.assertEqual(records, [])


if __name__ == "__main__":
    unittest.main()
