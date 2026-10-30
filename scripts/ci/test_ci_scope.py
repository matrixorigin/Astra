#!/usr/bin/env python3

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from unittest.mock import patch

from ci_scope import classify, main, select_macos_runner


class CiScopeTests(unittest.TestCase):
    def test_macos_allowlist_uses_author_id_including_bots_and_login_renames(self):
        for author_id in (39627130, 52204121, 49699333, 523063, 134036682):
            with self.subTest(author_id=author_id):
                event = {"pull_request": {"user": {"id": author_id, "login": "renamed-login"}},
                         "sender": {"id": 999999999}}
                self.assertEqual(select_macos_runner("pull_request", event),
                                 ["self-hosted", "macOS", "ARM64", "mac-arm64-mini"])

    def test_macos_unknown_author_stays_hosted_even_when_actor_or_login_matches(self):
        for user in ({}, {"login": "XuPeng-SH"}, {"id": 999999999, "login": "XuPeng-SH"},
                     {"id": "39627130"}, {"id": True}, None):
            with self.subTest(user=user):
                event = {"pull_request": {"user": user}, "sender": {"id": 39627130}}
                self.assertEqual(select_macos_runner("pull_request", event),
                                 ["macos-15"])
        for event in ({}, {"pull_request": None}, []):
            self.assertEqual(select_macos_runner("pull_request", event), ["macos-15"])

    def test_other_pushes_and_manual_events_remain_hosted(self):
        for event_name in ("workflow_dispatch", "pull_request_target"):
            self.assertEqual(select_macos_runner(event_name,
                {"ref": "refs/heads/main", "sender": {"id": 39627130}}), ["macos-15"])
        self.assertEqual(select_macos_runner("push", {"ref": "refs/heads/main"}), ["macos-15"])
        self.assertEqual(select_macos_runner("push", {"ref": "refs/heads/main"}, main_enabled=True),
                         ["self-hosted", "macOS", "ARM64", "mac-arm64-main"])
        for enabled in (False, True):
            for ref in ("refs/heads/develop", "refs/heads/feature", "refs/tags/v1.0", "main", None):
                with self.subTest(enabled=enabled, ref=ref):
                    self.assertEqual(select_macos_runner("push", {"ref": ref}, main_enabled=enabled),
                                     ["macos-15"])

    def test_macos_missing_event_routes_to_hosted_without_omitting_scope_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with patch.dict(os.environ, {"GITHUB_EVENT_PATH": str(Path(directory) / "missing"),
                                          "GITHUB_OUTPUT": str(output)}, clear=True), \
                 patch.object(sys, "argv", ["ci_scope.py", "crates/core/src/lib.rs"]), \
                 redirect_stdout(StringIO()), redirect_stderr(StringIO()):
                self.assertEqual(main(), 0)
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(json.loads(values["macos_runner"]), ["macos-15"])
            self.assertEqual(values["test_core"], "true")

    def assert_rename_selects_expected_scopes(
        self,
        source: str,
        destination: str,
        expected_scopes: set[str],
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args], text=True,
                    stderr=subprocess.PIPE,
                ).strip()

            git("init", "-q")
            git("config", "user.name", "CI scope regression")
            git("config", "user.email", "ci-scope@example.invalid")
            git("config", "diff.renames", "true")
            git("config", "commit.gpgsign", "false")
            original = root / source
            original.parent.mkdir(parents=True)
            original.write_text("// unchanged source moved across CI scopes\n")
            git("add", ".")
            git("commit", "-qm", "baseline")
            base = git("rev-parse", "HEAD")
            (root / destination).parent.mkdir(parents=True, exist_ok=True)
            git("mv", source, destination)
            git("commit", "-qm", "move source")
            head = git("rev-parse", "HEAD")
            self.assertTrue(git("diff", "--name-status", base, head).startswith("R100"))

            git("switch", "-q", "--detach", base)
            git("merge", "-q", "--no-ff", head, "-m", "test merge")
            merge = git("rev-parse", "HEAD")

            events = [
                ({"before": base, "after": head}, head),
                (
                    {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}},
                    merge,
                ),
            ]
            for event, github_sha in events:
                with self.subTest(event=next(iter(event))):
                    event_path = root / "event.json"
                    event_path.write_text(json.dumps(event))
                    output_path = root / "scope-output"
                    output_path.write_text("")
                    result = subprocess.run(
                        [sys.executable, str(Path(__file__).with_name("ci_scope.py").resolve())],
                        cwd=root,
                        env={
                            **os.environ,
                            "GITHUB_EVENT_PATH": str(event_path),
                            "GITHUB_OUTPUT": str(output_path),
                            "GITHUB_SHA": github_sha,
                        },
                        text=True, capture_output=True, check=True,
                    )
                    self.assertNotIn("enabling every CI scope", result.stderr)
                    enabled_scopes = {
                        name
                        for name, value in (
                            line.split("=", maxsplit=1)
                            for line in output_path.read_text().splitlines()
                        )
                        if value == "true"
                    }
                    self.assertEqual(enabled_scopes, expected_scopes)

    def test_sdk_to_web_rename_keeps_sdk_validation(self) -> None:
        self.assert_rename_selects_expected_scopes(
            "packages/sdk/src/moved.ts",
            "web/lib/moved.ts",
            {"sdk", "web", "online_integration"},
        )

    def test_runtime_to_cli_rename_keeps_runtime_validation(self) -> None:
        self.assert_rename_selects_expected_scopes(
            "crates/runtime/src/moved.rs",
            "crates/astra-cli/src/moved.rs",
            {
                "rust",
                "test_runtime",
                "test_cli",
                "test_core",
                "online_core",
                "online_integration",
            },
        )

    def test_pull_request_scope_uses_the_tested_merge_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args],
                    text=True,
                    stderr=subprocess.PIPE,
                ).strip()

            git("init", "-q", "-b", "main")
            git("config", "user.name", "CI scope regression")
            git("config", "user.email", "ci-scope@example.invalid")
            git("config", "commit.gpgsign", "false")
            (root / "README.md").write_text("baseline\n")
            git("add", ".")
            git("commit", "-qm", "baseline")
            common = git("rev-parse", "HEAD")

            git("switch", "-qc", "feature", common)
            (root / "README.md").write_text("documentation-only pull request\n")
            git("commit", "-qam", "update documentation")
            head = git("rev-parse", "HEAD")

            git("switch", "-q", "main")
            rust_source = root / "crates/core/src/lib.rs"
            rust_source.parent.mkdir(parents=True)
            rust_source.write_text("pub fn base_only() {}\n")
            git("add", ".")
            git("commit", "-qm", "advance base with Rust change")
            base = git("rev-parse", "HEAD")
            git("merge", "-q", "--no-ff", "feature", "-m", "test merge")
            merge = git("rev-parse", "HEAD")

            event_path = root / "event.json"
            event_path.write_text(json.dumps({
                "pull_request": {
                    "base": {"sha": base},
                    "head": {"sha": head},
                },
            }))
            output_path = root / "scope-output"
            output_path.write_text("")
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("ci_scope.py").resolve())],
                cwd=root,
                env={
                    **os.environ,
                    "GITHUB_EVENT_PATH": str(event_path),
                    "GITHUB_OUTPUT": str(output_path),
                    "GITHUB_SHA": merge,
                },
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertNotIn("enabling every CI scope", result.stderr)
            self.assertIn("changed_paths=README.md", result.stdout)
            self.assertFalse(any(
                line.endswith("=true") for line in output_path.read_text().splitlines()
            ))

    def test_pull_request_scope_rejects_an_unverified_checkout_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", "-C", str(root), *args],
                    text=True,
                    stderr=subprocess.PIPE,
                ).strip()

            git("init", "-q")
            git("config", "user.name", "CI scope regression")
            git("config", "user.email", "ci-scope@example.invalid")
            git("config", "commit.gpgsign", "false")
            (root / "README.md").write_text("baseline\n")
            git("add", ".")
            git("commit", "-qm", "baseline")
            base = git("rev-parse", "HEAD")
            (root / "README.md").write_text("head\n")
            git("commit", "-qam", "head")
            head = git("rev-parse", "HEAD")

            event_path = root / "event.json"
            event_path.write_text(json.dumps({
                "pull_request": {
                    "base": {"sha": base},
                    "head": {"sha": head},
                },
            }))
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("ci_scope.py").resolve())],
                cwd=root,
                env={
                    **os.environ,
                    "GITHUB_EVENT_PATH": str(event_path),
                    "GITHUB_SHA": head,
                },
                text=True,
                capture_output=True,
                check=True,
            )

            self.assertIn("enabling every CI scope", result.stderr)
            self.assertIn("rust=true", result.stdout)
            self.assertIn("online_integration=true", result.stdout)

    def test_documentation_and_governance_use_no_heavy_scope(self) -> None:
        scopes = classify(
            ["README.md", "docs/guides/testing.md", "CITATION.cff", ".github/CODEOWNERS"]
        )
        self.assertFalse(any(scopes.values()))

    def test_dot_prefixed_paths_are_not_corrupted(self) -> None:
        scopes = classify(["./.agent/skills/verify_task/SKILL.md"])
        self.assertFalse(any(scopes.values()))

    def test_sdk_change_also_validates_web_consumer(self) -> None:
        scopes = classify(["packages/sdk/src/client.ts"])
        self.assertTrue(scopes["sdk"])
        self.assertTrue(scopes["web"])
        self.assertFalse(scopes["rust"])
        self.assertTrue(scopes["online_integration"])
        self.assertFalse(scopes["online_core"])

    def test_web_change_selects_real_next_integration_without_unrelated_lanes(self) -> None:
        scopes = classify(["web/app/page.tsx"])
        self.assertTrue(scopes["web"])
        self.assertFalse(scopes["sdk"])
        self.assertFalse(scopes["rust"])
        self.assertTrue(scopes["online_integration"])
        self.assertFalse(scopes["online_core"])

    def test_cli_source_only_selects_cli_tests(self) -> None:
        scopes = classify(["crates/astra-cli/src/main.rs"])
        self.assertTrue(scopes["rust"])
        self.assertTrue(scopes["test_cli"])
        self.assertFalse(scopes["test_runtime"])
        self.assertFalse(scopes["online_core"])
        self.assertFalse(scopes["online_integration"])

    def test_terminal_reflow_harness_selects_cli_pty_lane(self) -> None:
        for path in ("scripts/tui-reflow/resize.cjs", "scripts/tui-reflow/pty_bridge.py",
                     "scripts/tui-reflow/package-lock.json"):
            with self.subTest(path=path):
                scopes = classify([path])
                self.assertTrue(scopes["test_cli"])
                self.assertEqual(sum(scopes.values()), 1)
        self.assertTrue(classify([".nvmrc"])["test_cli"])
        self.assertTrue(classify([".nvmrc"])["online_integration"])

    def test_test_only_rust_change_selects_owning_shard(self) -> None:
        scopes = classify(["crates/core/tests/repo_layout_contract.rs"])
        self.assertTrue(scopes["rust"])
        self.assertTrue(scopes["test_core"])
        self.assertFalse(scopes["test_cli"])
        self.assertFalse(scopes["online_core"])
        self.assertFalse(scopes["online_integration"])

    def test_shared_core_source_selects_downstream_lanes(self) -> None:
        scopes = classify(["crates/astra-text-utils/src/lib.rs"])
        for name in (
            "rust",
            "test_cli",
            "test_runtime",
            "test_services",
            "test_core",
            "online_core",
            "online_integration",
        ):
            self.assertTrue(scopes[name])

    def test_runtime_integration_test_selects_only_its_online_lane(self) -> None:
        scopes = classify(["crates/runtime/tests/http_contract.rs"])
        self.assertTrue(scopes["test_runtime"])
        self.assertFalse(scopes["test_cli"])
        self.assertFalse(scopes["online_core"])
        self.assertTrue(scopes["online_integration"])

    def test_services_test_selects_online_core_lane(self) -> None:
        scopes = classify(["crates/services/tests/work_repository_db_it.rs"])
        self.assertTrue(scopes["test_services"])
        self.assertTrue(scopes["online_core"])
        self.assertFalse(scopes["online_integration"])

    def test_harness_change_uses_targeted_contract_scope(self) -> None:
        scopes = classify(["scripts/harness/local_gateway_contract.sh"])
        self.assertTrue(scopes["harness"])
        self.assertFalse(scopes["script_contracts"])
        self.assertFalse(scopes["rust"])

    def test_offline_script_change_uses_targeted_contract_scope(self) -> None:
        for path in (
            "scripts/e2e/validate_capability_matrix.py",
            "scripts/load/db_capacity_report.py",
            "scripts/schema/schema_inventory.py",
        ):
            with self.subTest(path=path):
                scopes = classify([path])
                self.assertTrue(scopes["script_contracts"])
                self.assertFalse(scopes["harness"])
                self.assertFalse(scopes["rust"])

    def test_unrelated_script_remains_lightweight(self) -> None:
        scopes = classify(["scripts/render_readme_diagrams.py"])
        self.assertFalse(any(scopes.values()))

    def test_cargo_lock_runs_rust_but_not_node(self) -> None:
        scopes = classify(["Cargo.lock"])
        self.assertTrue(
            all(scopes[name] for name in ("rust", "test_cli", "online_core", "online_integration"))
        )
        self.assertFalse(scopes["sdk"])
        self.assertFalse(scopes["web"])

    def test_vendor_explicitly_selects_rust_without_unknown_path_fallback(self) -> None:
        for path in ("vendor/crossterm/src/event.rs", "vendor/crossterm/Cargo.lock", "vendor/new-crate/src/lib.rs"):
            with self.subTest(path=path):
                scopes = classify([path])
                for name in ("rust", "test_cli", "test_core", "test_services", "test_runtime", "online_core", "online_integration"):
                    self.assertTrue(scopes[name], name)
                for name in ("sdk", "web", "harness", "script_contracts"):
                    self.assertFalse(scopes[name], name)

    def test_docker_inputs_select_existing_rust_layout_contracts(self) -> None:
        for path in ("Dockerfile", ".dockerignore"):
            with self.subTest(path=path):
                scopes = classify([path])
                self.assertTrue(scopes["rust"])
                self.assertTrue(scopes["test_core"])
                self.assertFalse(scopes["sdk"])
                self.assertFalse(scopes["web"])

    def test_ci_classifier_change_falls_back_to_every_scope(self) -> None:
        self.assertTrue(all(classify(["scripts/ci/ci_scope.py"]).values()))

    def test_known_governance_change_uses_only_lightweight_checks(self) -> None:
        self.assertFalse(any(classify(["LICENSE"]).values()))

    def test_unknown_change_falls_back_to_every_scope(self) -> None:
        self.assertTrue(all(classify(["unexpected/build.graph"]).values()))

    def test_missing_diff_falls_back_to_every_scope(self) -> None:
        self.assertTrue(all(classify([]).values()))

    def test_detection_error_exits_successfully_with_full_fallback(self) -> None:
        stdout = StringIO()
        stderr = StringIO()
        with (
            patch.dict("os.environ", {}, clear=True),
            patch("sys.argv", ["ci_scope.py"]),
            redirect_stdout(stdout),
            redirect_stderr(stderr),
        ):
            self.assertEqual(main(), 0)
        self.assertIn("rust=true", stdout.getvalue())
        self.assertIn("online_integration=true", stdout.getvalue())
        self.assertIn("enabling every CI scope", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
