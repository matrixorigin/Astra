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


class CliArchiveTests(unittest.TestCase):
    def setUp(self):
        self.workflow = (ROOT / ".github/workflows/test.yml").read_text()
        self.producer = self.workflow.split("\n  cli-test-build:\n", 1)[1].split("\n  shard-a:\n", 1)[0]
        self.consumer = self.workflow.split("\n  shard-a:\n", 1)[1].split("\n  shard-b:\n", 1)[0]

    def test_required_labels_have_one_shell_and_four_complementary_partitions(self):
        matrix = self.consumer.split("        include:\n", 1)[1].split("    steps:\n", 1)[0]
        rows = re.findall(
            r'- segment: ([\w-]+)\n\s+filter: "([^"]+)"\n\s+partition: ([^\n]+)', matrix
        )
        self.assertEqual(len(rows), 5)
        self.assertEqual({row[0] for row in rows}, {
            "non-edge", "edge-shell", "edge-fs-tools", "edge-git-gix", "edge-rest",
        })
        shell = [row for row in rows if row[0] == "edge-shell"]
        self.assertEqual(shell, [("edge-shell", "test(/edge_tools::shell::/)", '""')])
        others = [row for row in rows if row[0] != "edge-shell"]
        self.assertEqual({row[1] for row in others}, {"not test(/edge_tools::shell::/)"})
        self.assertEqual(sorted(row[2] for row in others), [f"hash:{i}/4" for i in range(1, 5)])
        self.assertIn('name: "Test: astra-cli (${{ matrix.segment }})"', self.consumer)

    def test_failed_producer_is_rejected_before_archive_execution(self):
        gate = self.consumer.split("      - name: Require shared CLI build\n", 1)[1].split("      - ", 1)[0]
        self.assertIn("if: env.RUN_TESTS == 'true'", gate)
        self.assertIn("BUILD_RESULT: ${{ needs.cli-test-build.result }}", gate)
        script = gate.split("        run: ", 1)[1].strip()
        for status in ("success", "failure", "cancelled", "skipped", ""):
            with self.subTest(status=status):
                result = subprocess.run(["bash", "-c", script],
                    env={**os.environ, "BUILD_RESULT": status}, capture_output=True)
                self.assertEqual(result.returncode == 0, status == "success")
        self.assertLess(self.consumer.index("Require shared CLI build"), self.consumer.index("actions/download-artifact@"))
        self.assertIn("needs: [scope, cli-test-build]", self.consumer)
        self.assertIn("if: ${{ !cancelled() }}", self.consumer)
        for section in (self.producer, self.consumer):
            self.assertIn("needs.scope.result != 'success'", section)
            self.assertIn("needs.scope.outputs.test_cli == 'true'", section)
        for job in ("shard-b", "shard-c", "shard-d"):
            header = self.workflow.split(f"\n  {job}:\n", 1)[1].split("    steps:\n", 1)[0]
            self.assertIn("needs: scope", header)
            self.assertNotIn("cli-test-build", header)

    def test_archive_handoff_is_revision_bound_and_consumers_do_not_build(self):
        identity = "name: cli-tests-${{ github.sha }}-${{ github.run_id }}"
        self.assertEqual(self.producer.count(identity), 1)
        self.assertEqual(self.consumer.count(identity), 1)
        self.assertIn("if-no-files-found: error", self.producer)
        self.assertIn("overwrite: true", self.producer)
        self.assertNotIn("overwrite:", self.consumer)
        self.assertIn("retention-days: 1", self.producer)
        self.assertIn('build-tools: "false"', self.consumer)
        self.assertNotRegex(self.consumer, r"cargo\s+(?:build|test|nextest\s+archive)\b")
        download = self.consumer.split("      - uses: actions/download-artifact@", 1)[1].split("      - name:", 1)[0]
        self.assertIn("if: env.RUN_TESTS == 'true'", download)
        self.assertNotIn("continue-on-error", download)
        self.assertNotIn("run-id:", download)
        self.assertIn("cancel-in-progress: true", self.workflow)
        setup = (ROOT / ".github/actions/rust-setup/action.yml").read_text()
        for marker in ("- name: Free disk space",
                       "- name: Install mold linker", "- name: Install sccache",
                       "- uses: Swatinem/rust-cache@"):
            block = setup.split(marker, 1)[1].split("\n    - ", 1)[0]
            self.assertIn("inputs.build-tools == 'true'", block)
        self.assertRegex(setup, r"tool: cargo-nextest@\d+\.\d+\.\d+")

    def test_consumer_shell_preserves_partition_remap_and_fails_without_fallback(self):
        script = workflow_run_script(".github/workflows/test.yml", '"Test astra-cli (${{ matrix.segment }})"')
        for partition in ("", "hash:1/4", "hash:2/4", "hash:3/4", "hash:4/4"):
            for archive_present, failed_run in ((True, False), (False, False), (True, True)):
                with self.subTest(partition=partition, archive=archive_present, failed=failed_run), tempfile.TemporaryDirectory() as directory:
                    fixture = Path(directory)
                    (fixture / "target").mkdir()
                    if archive_present:
                        (fixture / "target/astra-cli-tests.tar.zst").touch()
                    stub = fixture / "cargo-nextest"
                    stub.write_text(f"#!{sys.executable}\n" + '''
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open("calls.jsonl", "a") as output:
    output.write(json.dumps(args) + "\\n")
if args[:2] != ["nextest", "run"] or "--archive-file" not in args:
    sys.exit(99)
if not Path(args[args.index("--archive-file") + 1]).is_file():
    sys.exit(44)
sys.exit(45 if os.environ["FAIL_RUN"] == "1" else 0)
''')
                    stub.chmod(0o755)
                    # Never let a regression invoke the host's real Cargo.
                    cargo_guard = fixture / "cargo"
                    cargo_guard.write_text(stub.read_text())
                    cargo_guard.chmod(0o755)
                    result = subprocess.run(["bash", "-c", script], cwd=fixture, env={
                        **os.environ, "PATH": f"{fixture}{os.pathsep}{os.environ['PATH']}",
                        "NEXTEST_PARTITION": partition, "NEXTEST_FILTER": "test(/edge_tools::shell::/)" if not partition else "not test(/edge_tools::shell::/)",
                        "COMMAND_TIMEOUT": "5s", "GITHUB_WORKSPACE": str(fixture / "workspace with spaces"),
                        "FAIL_RUN": "1" if failed_run else "0",
                    }, capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 0 if archive_present and not failed_run else (45 if failed_run else 44), result.stderr)
                    calls = [json.loads(line) for line in (fixture / "calls.jsonl").read_text().splitlines()]
                    self.assertEqual(len(calls), 1, "Archive errors must not trigger fallback builds/runs")
                    args = calls[0]
                    self.assertEqual(args[args.index("--workspace-remap") + 1], str(fixture / "workspace with spaces"))
                    self.assertEqual(args[args.index("--extract-to") + 1], "target/cli-test-extract")
                    self.assertEqual(args[args.index("--profile") + 1], "ci")
                    self.assertEqual(args[args.index("-E") + 1], "test(/edge_tools::shell::/)" if not partition else "not test(/edge_tools::shell::/)")
                    if partition:
                        self.assertEqual(args[args.index("--partition") + 1], partition)
                    else:
                        self.assertNotIn("--partition", args)


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
            with self.subTest(status=status):
                result = subprocess.run(["bash", "-c", script], env={
                    **os.environ, "SHARDS_RESULT": status,
                }, capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, status == "success")

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
        self.assertIn("needs: test-online", core)
        self.assertIn("needs.test-online.result", core)
        self.assertIn("if: ${{ !cancelled() }}", core)


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
