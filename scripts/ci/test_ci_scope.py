#!/usr/bin/env python3

import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from unittest.mock import patch

from ci_scope import classify, main


class CiScopeTests(unittest.TestCase):
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

    def test_web_change_does_not_run_sdk_or_rust(self) -> None:
        scopes = classify(["web/app/page.tsx"])
        self.assertTrue(scopes["web"])
        self.assertFalse(scopes["sdk"])
        self.assertFalse(scopes["rust"])

    def test_cli_source_only_selects_cli_tests(self) -> None:
        scopes = classify(["crates/astra-cli/src/main.rs"])
        self.assertTrue(scopes["rust"])
        self.assertTrue(scopes["test_cli"])
        self.assertFalse(scopes["test_runtime"])
        self.assertFalse(scopes["online_core"])
        self.assertFalse(scopes["online_integration"])

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
        self.assertFalse(scopes["rust"])

    def test_cargo_lock_runs_rust_but_not_node(self) -> None:
        scopes = classify(["Cargo.lock"])
        self.assertTrue(
            all(scopes[name] for name in ("rust", "test_cli", "online_core", "online_integration"))
        )
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
