#!/usr/bin/env python3
"""Guard workspace shard coverage and Docker's vendored path dependency inputs."""

from pathlib import Path
import os
import re
import shlex
import subprocess
import tempfile
import textwrap
import tomllib
import unittest

from ci_scope import classify
from test_release_build_shells import workflow_run_script


ROOT = Path(__file__).resolve().parents[2]


class BuildCoverageTests(unittest.TestCase):
    def test_rust_setup_disk_guard_uses_workspace_free_space_without_real_cleanup(self):
        action = (ROOT / ".github/actions/rust-setup/action.yml").read_text()
        section = action.split("\n    - name: Guard workspace disk space\n", 1)[1]
        run_block = section.split("\n    - name:", 1)[0]
        script = textwrap.dedent(run_block.split("\n      run: |\n", 1)[1])
        threshold_kib = 30 * 1024 * 1024
        cleanup = [
            "rm -rf /usr/share/dotnet",
            "rm -rf /usr/local/lib/android",
            "rm -rf /opt/ghc",
            "rm -rf /opt/hostedtoolcache/CodeQL",
            "docker image prune -af",
        ]

        for mode, available_kib, should_skip in (
            ("above", threshold_kib + 1, True),
            ("equal", threshold_kib, True),
            ("below", threshold_kib - 1, False),
            ("invalid", "not-a-number", False),
            ("failure", None, False),
        ):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                mock_bin = root / "mock-bin"
                mock_bin.mkdir()
                workspace = root / "workspace"
                workspace.mkdir()
                sudo_log = root / "sudo.log"

                (mock_bin / "df").write_text(
                    """#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"-Pk"* ]]; then
  case "${MOCK_DF_MODE}" in
    above|equal|below|invalid)
      printf '%s\\n' 'Filesystem 1024-blocks Used Available Capacity Mounted on'
      printf 'mock 100 10 %s 1%% /workspace\\n' "${MOCK_DF_AVAILABLE_KIB}"
      ;;
    failure)
      printf '%s\\n' 'mock df failure' >&2
      exit 7
      ;;
  esac
else
  printf '%s\\n' 'Filesystem Size Used Avail Capacity Mounted on'
  printf '%s\\n' 'mock 100G 1G 99G 1% /workspace'
fi
"""
                )
                (mock_bin / "sudo").write_text(
                    """#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$*" >> "${MOCK_SUDO_LOG}"
"""
                )
                for command in (mock_bin / "df", mock_bin / "sudo"):
                    command.chmod(0o755)

                environment = {
                    **os.environ,
                    "GITHUB_WORKSPACE": str(workspace),
                    "MOCK_DF_MODE": mode,
                    "MOCK_DF_AVAILABLE_KIB": str(available_kib or ""),
                    "MOCK_SUDO_LOG": str(sudo_log),
                    "PATH": f"{mock_bin}{os.pathsep}{os.environ['PATH']}",
                }
                result = subprocess.run(
                    ["bash", "-c", script],
                    env=environment,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                output = result.stdout + result.stderr
                decision = "skip cleanup" if should_skip else "run cleanup"
                self.assertIn(f"Disk guard decision: {decision}", output)
                self.assertIn(
                    f"threshold={threshold_kib} KiB (30 GiB)",
                    output,
                )

                invocations = sudo_log.read_text().splitlines() if sudo_log.exists() else []
                self.assertEqual(invocations, [] if should_skip else cleanup)

    def test_make_lint_preserves_restored_artifacts_and_checks_all_targets(self):
        result = subprocess.run(
            ["make", "--no-print-directory", "-n", "lint", "CARGO=cargo"],
            cwd=ROOT,
            env={**os.environ, "MAKEFLAGS": "", "MFLAGS": ""},
            capture_output=True,
            text=True,
            check=True,
        )
        commands = result.stdout
        clippy = next(
            shlex.split(line) for line in commands.splitlines()
            if line.startswith("cargo clippy ")
        )
        self.assertIn("--all-targets", clippy)
        self.assertEqual(clippy[clippy.index("--") + 1:], ["-D", "warnings"])
        self.assertNotRegex(
            commands,
            r"(?i)\bsweep\b|\bfind\b[^\n]*(?:-mmin|-mtime|-delete)|\bcargo\s+clean\b",
            "Lint must not age-delete restored build artifacts before checking",
        )

    def test_every_workspace_crate_has_an_offline_test_shard(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        assigned = set()
        for job in ("cli-test-build", "shard-b", "shard-c", "shard-d"):
            section = workflow.split(f"\n  {job}:\n", 1)[1]
            section = re.split(r"\n  [a-z][a-z-]*:\n", section, maxsplit=1)[0]
            for command in re.findall(r"cargo nextest run\b(.*?)(?=\n      -|\Z)", section, re.S):
                assigned.update(re.findall(r"-p\s+([\w-]+)", command))
        packages = {
            tomllib.loads((ROOT / member / "Cargo.toml").read_text())["package"]["name"]
            for member in workspace["members"]
        }
        self.assertEqual(packages - assigned, set(), "Workspace crates missing from CI test shards")

    def test_complete_cli_run_prebuilds_required_standalone_mcp_fixture(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        producer = workflow.split("\n  cli-test-build:\n", 1)[1].split("\n  shard-a:\n", 1)[0]
        self.assertIn("cargo build --locked -p astra-cli --bin mock_mcp_server", producer)
        test_command = re.search(r"run: (cargo nextest run[^\n]+)", producer).group(1)
        args = shlex.split(test_command)
        self.assertEqual(args[args.index("-p") + 1], "astra-cli")
        self.assertTrue({"--locked", "--lib", "--bins"}.issubset(args))
        self.assertNotIn("-E", args, "Run the complete CLI inventory")
        self.assertNotIn("--partition", args)
        self.assertEqual(args[args.index("--profile") + 1], "ci")
        self.assertLess(producer.index("Prebuild mock MCP server"), producer.index(test_command))

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
        self.assertIn("segment: [default-reader, dev-tty-reader, reflow]", pty)
        for segment, step, features in (
            ("default-reader", "Test default terminal reader through PTYs", ""),
            ("dev-tty-reader", "Test dev-tty terminal reader through PTYs", "--features crossterm/use-dev-tty "),
        ):
            with self.subTest(segment=segment):
                commands = workflow_run_script(".github/workflows/test.yml", step).strip()
                self.assertEqual(commands,
                    f"cargo test --locked -p astra-cli --lib {features}tui::terminal_startup -- --test-threads=2")
                block = pty.split(f"      - name: {step}\n", 1)[1].split("      - ", 1)[0]
                self.assertIn(f"if: matrix.segment == '{segment}'", block)

    def test_terminal_reflow_uses_real_binary_and_locked_emulator(self):
        commands = workflow_run_script(".github/workflows/test.yml", "Test inline terminal resize with xterm reflow")
        self.assertIn("cargo build --locked -p astra-cli --bin astra", commands)
        self.assertIn("npm ci --ignore-scripts --prefix scripts/tui-reflow", commands)
        self.assertIn('ASTRA_TEST_BINARY="$PWD/target/debug/astra" npm test --prefix scripts/tui-reflow', commands)

    def test_macos_cli_bash_reuses_default_reader_without_losing_coordination_contracts(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        terminal = workflow.split("\n  terminal-pty-shards:\n", 1)[1].split("\n  macos-workspace-coordination:\n", 1)[0]
        self.assertEqual(workflow.count("- name: Test macOS CLI Bash execution"), 1)
        bash = terminal.split("      - name: Test macOS CLI Bash execution\n", 1)[1].split("      - ", 1)[0]
        self.assertIn("if: matrix.os == 'macos-15' && matrix.segment == 'default-reader'", bash)
        self.assertIn("--lib edge_tools::shell::tests::bash_echo_returns_output -- --exact", bash)
        self.assertLess(terminal.index("Test default terminal reader through PTYs"), terminal.index("Initialize system Git for macOS CLI observation"))
        self.assertLess(terminal.index("Initialize system Git for macOS CLI observation"), terminal.index("Test macOS CLI Bash execution"))
        contracts = workflow.split("\n  macos-workspace-coordination-tests:\n", 1)[1].split("\n  test-online-core:", 1)[0]
        self.assertIn('install-nextest: "false"', contracts)
        self.assertIn("-p astra-tools", contracts)
        self.assertIn("--all-targets -- -D warnings", contracts)
        self.assertIn("workspace_observation::tests:: -- --test-threads=1", contracts)
        self.assertIn("/usr/bin/git", contracts)
        self.assertNotIn("-p astra-cli", contracts, "Do not compile the same CLI test executable twice on macOS")

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
                    self.assertIn(str(root / missing), result.stderr)
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
