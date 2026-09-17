#!/usr/bin/env python3
"""Guard workspace shard coverage and Docker's vendored path dependency inputs."""

from pathlib import Path
import os
import re
import subprocess
import tempfile
import tomllib
import unittest

from ci_scope import classify
from test_release_build_shells import workflow_run_script


ROOT = Path(__file__).resolve().parents[2]


class BuildCoverageTests(unittest.TestCase):
    def test_every_workspace_crate_has_an_offline_test_shard(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        assigned = set()
        for shard in ("a", "b", "c", "d"):
            section = workflow.split(f"\n  shard-{shard}:\n", 1)[1]
            section = re.split(r"\n  [a-z][a-z-]*:\n", section, maxsplit=1)[0]
            for command in re.findall(r"cargo nextest run\b(.*?)(?=\n      -|\Z)", section, re.S):
                assigned.update(re.findall(r"-p\s+([\w-]+)", command))
        packages = {
            tomllib.loads((ROOT / member / "Cargo.toml").read_text())["package"]["name"]
            for member in workspace["members"]
        }
        self.assertEqual(packages - assigned, set(), "Workspace crates missing from CI test shards")

    def test_vendored_terminal_units_are_locked_and_pty_tests_run_both_readers(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        section = workflow.split("\n  vendored-terminal:\n", 1)[1].split("\n  terminal-pty:", 1)[0]
        self.assertIn("os: [ubuntu-latest, macos-15]", section)
        for features in ("event-stream", "event-stream,use-dev-tty"):
            self.assertIn(f"cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features {features}\n", section)
        lock = tomllib.loads((ROOT / "vendor/crossterm/Cargo.lock").read_text())
        self.assertTrue(any(p["name"] == "crossterm" for p in lock["package"]))
        self.assertNotIn("/Cargo.lock", (ROOT / "vendor/crossterm/.gitignore").read_text().splitlines())
        pty = workflow.split("\n  terminal-pty:\n", 1)[1].split("\n  macos-workspace-coordination:", 1)[0]
        self.assertIn("os: [ubuntu-latest, macos-15]", pty)
        commands = workflow_run_script(".github/workflows/test.yml", "Test real terminal readers through PTYs").splitlines()
        self.assertEqual(commands, [
            "cargo test --locked -p astra-cli --lib tui::terminal_startup -- --test-threads=2",
            "cargo test --locked -p astra-cli --lib --features crossterm/use-dev-tty tui::terminal_startup -- --test-threads=2",
        ])

    def test_ci_resolves_the_actual_builder_base_not_only_the_planner(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        self.assertIn("FROM dependency-inputs AS builder", dockerfile)
        stage = dockerfile.split("FROM chef AS dependency-inputs\n", 1)[1].split("FROM dependency-inputs AS builder", 1)[0]
        self.assertIn("--no-build", stage)
        self.assertIn("cargo metadata --locked --format-version 1", stage)
        self.assertNotIn("--no-deps", stage.split("RUN ", 1)[1])

    def test_actual_dependency_resolution_rejects_missing_arbitrary_patch_paths(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        stage = dockerfile.split("FROM chef AS dependency-inputs\n", 1)[1].split("FROM dependency-inputs AS builder", 1)[0]
        script = stage.split("RUN ", 1)[1].replace("\\\n", " ").strip()
        # The fixture is already hydrated, so stub only chef, not Cargo's real
        # dependency resolver. Run the production stage command against patches
        # both inside and outside vendor, without network or compilation.
        stub = 'cargo() { if [ "$1" = chef ]; then return 0; fi; command cargo "$@"; }\n'
        for missing in (None, "vendor/foo", "local/bar"):
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "Cargo.toml").write_text('''[workspace]
members = ["app"]
exclude = ["vendor/foo", "local/bar"]
resolver = "2"
[patch.crates-io]
foo = { path = "vendor/foo" }
bar = { path = "local/bar" }
''')
                for name, path in (("fixture", "app"), ("foo", "vendor/foo"), ("bar", "local/bar")):
                    package = root / path
                    (package / "src").mkdir(parents=True)
                    manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
                    if name == "fixture":
                        manifest += '[dependencies]\nfoo = "0.1"\nbar = "0.1"\n'
                    (package / "Cargo.toml").write_text(manifest)
                    (package / "src/lib.rs").write_text("")
                env = {**os.environ, "CARGO_NET_OFFLINE": "true", "RUSTC_WRAPPER": "",
                       "CARGO_TARGET_DIR": str(root / "target")}
                subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=root, env=env,
                               check=True, capture_output=True)
                if missing:
                    (root / missing).rename(root / "removed-patch")
                result = subprocess.run(["bash", "-ec", stub + script], cwd=root, env=env,
                                        capture_output=True, text=True)
                if missing:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("failed to load source", result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)

    def test_docker_context_changes_reach_shared_dependency_stage(self):
        for path in ("Dockerfile", ".dockerignore", "Cargo.toml", "Cargo.lock"):
            self.assertTrue(classify([path])["rust"], path)
        workflow = (ROOT / ".github/workflows/static-checks.yml").read_text()
        job = workflow.split("\n  docker-dependencies:\n", 1)[1].split("\n  check:", 1)[0]
        self.assertIn("needs.scope.outputs.rust == 'true'", job)
        self.assertIn("context: .", job)
        self.assertIn("target: dependency-inputs", job)
        self.assertIn("push: false", job)
        check = workflow.split("\n  check:\n", 1)[1].split("\n  lint:", 1)[0]
        self.assertIn("needs: [scope, lint, docker-dependencies]", check)
        lint = workflow.split("\n  lint:\n", 1)[1].split("\n    steps:", 1)[0]
        self.assertIn("needs: scope", lint)
        self.assertNotIn("docker-dependencies", lint)

    def test_required_check_aggregates_success_failure_and_legitimate_skips(self):
        script = workflow_run_script(".github/workflows/static-checks.yml", "Require static gates")
        for lint in ("success", "failure", "cancelled", "skipped"):
            for docker in ("success", "failure", "cancelled", "skipped"):
                for required in (True, False):
                    with self.subTest(lint=lint, docker=docker, required=required):
                        result = subprocess.run(["bash", "-c", script], env={
                            **os.environ, "LINT_RESULT": lint, "DOCKER_RESULT": docker,
                            "DOCKER_REQUIRED": str(required).lower(),
                        }, capture_output=True, text=True)
                        success = lint == "success" and (docker == "success" or (not required and docker == "skipped"))
                        self.assertEqual(result.returncode == 0, success)


if __name__ == "__main__":
    unittest.main()
