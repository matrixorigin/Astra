#!/usr/bin/env python3
"""Guard workspace shard coverage and Docker's vendored path dependency inputs."""

from pathlib import Path
from itertools import product
import json
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


def rust_setup_disk_guard():
    action = (ROOT / ".github/actions/rust-setup/action.yml").read_text()
    section = action.split("    - name: Guard workspace disk space\n", 1)[1].split("\n    - name:", 1)[0]
    return textwrap.dedent(section.split("      run: |\n", 1)[1])


class BuildCoverageTests(unittest.TestCase):
    def test_self_hosted_build_survives_checkout_cleanup_and_rechecks_source(self):
        action = (ROOT / ".github/actions/rust-setup/action.yml").read_text()
        self.assertIn("${{ runner.workspace }}/.astra-ci-target", action)
        cache = action.split("    - uses: Swatinem/rust-cache@", 1)[1]
        self.assertIn("runner.environment != 'self-hosted'", cache)
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            repo = workspace / "checkout"
            source = repo / "dep/src/lib.rs"
            source.parent.mkdir(parents=True)
            (repo / "Cargo.toml").write_text('[package]\nname="cache-fixture"\nversion="0.1.0"\nedition="2021"\n[dependencies]\ncache-dep={path="dep"}\n[features]\nalternate=["cache-dep/alternate"]\n')
            (repo / "dep/Cargo.toml").write_text('[package]\nname="cache-dep"\nversion="0.1.0"\nedition="2021"\n[features]\nalternate=[]\n')
            (repo / "src").mkdir()
            (repo / "src/main.rs").write_text('fn main() { println!("{}", cache_dep::value()); }')
            source.write_text('pub fn value() -> u8 { if cfg!(feature="alternate") { 9 } else { 1 } }')
            for args in (("init", "-q"), ("add", "."), ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")):
                subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True)
            github_env = workspace / "env"
            environment = {**os.environ, "GITHUB_ENV": str(github_env),
                           "GITHUB_WORKSPACE": str(repo), "ASTRA_CI_RUNNER_ENVIRONMENT": "self-hosted",
                           "ASTRA_CI_TARGET_DIR": str(workspace / ".astra-ci-target"),
                           "RUSTC_WRAPPER": "", "CARGO_INCREMENTAL": "0"}
            script = "df() { printf 'Filesystem Blocks Used Available Capacity Mount\\nfixture 100000000 0 100000000 0%% /\\n'; }\n" + rust_setup_disk_guard()
            subprocess.run(["bash", "-eu", "-c", script], env=environment, check=True, capture_output=True)
            environment.update(line.split("=", 1) for line in github_env.read_text().splitlines())
            for attempt in range(6):
                if attempt == 1:
                    stale = repo / "stale-untracked"
                    stale.touch()
                    subprocess.run(["git", "clean", "-ffdx"], cwd=repo, check=True, capture_output=True)
                    self.assertFalse(stale.exists())
                elif attempt == 4:
                    source.write_text("pub fn value() -> u8 { 2 }")
                elif attempt == 5:
                    subprocess.run(["git", "checkout", "--", "dep/src/lib.rs"], cwd=repo, check=True)
                args = ["cargo", "build", "--offline", "--message-format=json"]
                if attempt == 2:
                    args += ["--features", "alternate"]
                build = subprocess.run(args, cwd=repo, env=environment, capture_output=True, text=True)
                self.assertEqual(build.returncode, 0, build.stderr)
                artifacts = [json.loads(line) for line in build.stdout.splitlines()]
                dependency = next(a for a in artifacts if a.get("reason") == "compiler-artifact" and a["target"]["name"] == "cache_dep")
                self.assertEqual(dependency["fresh"], attempt in (1, 3))
                binary = Path(environment["CARGO_TARGET_DIR"]) / "debug/cache-fixture"
                result = subprocess.run([str(binary)], capture_output=True, text=True, check=True)
                self.assertEqual(result.stdout.strip(), "9" if attempt == 2 else "2" if attempt == 4 else "1")

    def test_rust_toolchain_bootstraps_missing_rust_and_reuses_cargo_bin(self):
        action = (ROOT / ".github/actions/rust-toolchain/action.yml").read_text()
        script = textwrap.dedent(action.split("      run: |\n", 1)[1])
        for installed, install_failure in ((False, False), (True, False), (False, True)):
            with self.subTest(installed=installed, failure=install_failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                mock_bin = root / "mock-bin"
                cargo_home = root / "cargo home"
                mock_bin.mkdir()
                (cargo_home / "bin").mkdir(parents=True)
                calls = root / "calls"
                github_path = root / "github-path"
                github_env = root / "github-env"
                outside = root / "outside-workspace"
                outside.mkdir()
                installer = root / "installer"
                installer.write_text("""#!/bin/sh
set -eu
printf 'installer %s\\n' "$*" >> "$MOCK_CALLS"
cp "$MOCK_BIN/rustup-source" "$CARGO_HOME/bin/rustup"
cp "$MOCK_BIN/rustc-source" "$CARGO_HOME/bin/rustc"
""")
                programs = {
                    "curl": 'printf "curl\\n" >> "$MOCK_CALLS"; if [ "$MOCK_FAILURE" = 1 ]; then exit 22; fi; cat "$MOCK_INSTALLER"',
                    "rustup-source": 'printf "rustup %s\\n" "$*" >> "$MOCK_CALLS"; if [ "$1" = show ]; then test -f "$CARGO_HOME/ready"; printf "1.97.0-fixture (overridden by repository)\\n"; else touch "$CARGO_HOME/ready"; fi',
                    "rustc-source": 'test -f "$CARGO_HOME/ready"; if [ "${RUSTUP_TOOLCHAIN:-}" != 1.97.0-fixture ] && [ "$PWD" != "$MOCK_WORKSPACE" ]; then exit 42; fi; printf "rustc pinned-fixture\\n"',
                    "sccache": 'compiler="$1"; shift; cd "$MOCK_OUTSIDE"; "$compiler" "$@"',
                }
                for name, body in programs.items():
                    path = mock_bin / name
                    path.write_text("#!/bin/sh\nset -eu\n" + body + "\n")
                    path.chmod(0o755)
                if installed:
                    for name in ("rustup", "rustc"):
                        target = cargo_home / "bin" / name
                        target.write_bytes((mock_bin / (name + "-source")).read_bytes())
                        target.chmod(0o755)
                    (cargo_home / "ready").touch()
                environment = {**os.environ, "PATH": str(mock_bin) + ":/usr/bin:/bin",
                         "CARGO_HOME": str(cargo_home), "GITHUB_PATH": str(github_path),
                         "GITHUB_ENV": str(github_env), "MOCK_WORKSPACE": str(ROOT),
                         "MOCK_OUTSIDE": str(outside), "SCCACHE_PATH": str(mock_bin / "sccache"),
                         "MOCK_BIN": str(mock_bin), "MOCK_CALLS": str(calls),
                         "MOCK_INSTALLER": str(installer), "MOCK_FAILURE": str(int(install_failure))}
                result = subprocess.run(
                    ["/bin/bash", "-e", "-c", script], cwd=ROOT, capture_output=True, text=True,
                    env=environment,
                )
                invocations = calls.read_text().splitlines()
                if install_failure:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn("rustc pinned-fixture", result.stdout)
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("rustc pinned-fixture", result.stdout)
                self.assertEqual(github_path.read_text().strip(), str(cargo_home / "bin"))
                self.assertEqual("curl" in invocations, not installed)
                self.assertEqual("rustup toolchain install" in invocations, not installed)
                selected = dict(line.split("=", 1) for line in github_env.read_text().splitlines())
                self.assertEqual(selected["RUSTUP_TOOLCHAIN"], "1.97.0-fixture")
                next_environment = {**environment, **selected,
                                    "PATH": str(cargo_home / "bin") + ":" + environment["PATH"]}
                probe = subprocess.run(["bash", str(ROOT / "scripts/ci/configure-sccache.sh")],
                    cwd=ROOT, env=next_environment, capture_output=True, text=True)
                self.assertEqual(probe.returncode, 0, probe.stderr)
                self.assertIn("RUSTC_WRAPPER=" + str(mock_bin / "sccache"), github_env.read_text())
                # Without the exported selection, the same proxy fails only
                # when sccache invokes it outside the repository. The compiler
                # itself still works in ROOT, reproducing the old fail-open path.
                negative_env = root / "negative-env"
                next_environment.pop("RUSTUP_TOOLCHAIN")
                next_environment["GITHUB_ENV"] = str(negative_env)
                probe = subprocess.run(["bash", str(ROOT / "scripts/ci/configure-sccache.sh")],
                    cwd=ROOT, env=next_environment, capture_output=True, text=True)
                self.assertEqual(probe.returncode, 0, probe.stderr)
                self.assertIn("Rust cache disabled", probe.stderr)
                self.assertTrue(negative_env.read_text().endswith("RUSTC_WRAPPER=\n"))
        for path in (".github/actions/rust-setup/action.yml", ".github/workflows/test.yml"):
            self.assertIn("uses: ./.github/actions/rust-toolchain", (ROOT / path).read_text())

    def test_rust_setup_disk_guard_uses_workspace_free_space_without_real_cleanup(self):
        script = rust_setup_disk_guard()
        threshold_kib = 30 * 1024 * 1024
        cleanup = [
            "rm -rf /usr/share/dotnet",
            "rm -rf /usr/local/lib/android",
            "rm -rf /opt/ghc",
            "rm -rf /opt/hostedtoolcache/CodeQL",
            "docker image prune -af",
        ]

        for (mode, available_kib, should_skip), runner_environment in product((
            ("above", threshold_kib + 1, True),
            ("equal", threshold_kib, True),
            ("below", threshold_kib - 1, False),
            ("invalid", "not-a-number", False),
            ("failure", None, False),
        ), ("github-hosted", "self-hosted")):
            with self.subTest(mode=mode, runner=runner_environment), tempfile.TemporaryDirectory() as directory:
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
                    "GITHUB_ENV": str(root / "github-env"),
                    "ASTRA_CI_TARGET_DIR": str(root / ".astra-ci-target"),
                    "ASTRA_CI_RUNNER_ENVIRONMENT": runner_environment,
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
                self_hosted_failure = runner_environment == "self-hosted" and not should_skip
                self.assertEqual(result.returncode, 1 if self_hosted_failure else 0, result.stderr)
                output = result.stdout + result.stderr
                decision = ("skip cleanup" if should_skip else
                            "fail self-hosted" if self_hosted_failure else "run cleanup")
                self.assertIn(f"Disk guard decision: {decision}", output)
                self.assertIn(
                    f"threshold={threshold_kib} KiB (30 GiB)",
                    output,
                )

                invocations = sudo_log.read_text().splitlines() if sudo_log.exists() else []
                self.assertEqual(invocations, [] if should_skip or self_hosted_failure else cleanup)

    def test_self_hosted_target_reclamation_preserves_neighbors_and_rechecks_disk(self):
        limit_kib = 64 * 1024 * 1024
        scenarios = (
            ("warm", limit_kib - 1, False, False, 0),
            ("at-limit", limit_kib, False, False, 0),
            ("oversized", limit_kib + 1, False, True, 0),
            ("low-disk", 1024, True, True, 0),
            ("still-low", 1024, True, True, 1),
            ("failed-recheck", 1024, True, True, 1),
            ("failed-cleanup", 1024, True, False, 7),
            ("bad-size", "invalid", False, False, 1),
            ("symlink", 0, True, True, 0),
        )
        for name, size_kib, low_disk, reclaimed, status in scenarios:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                target = root / ".astra-ci-target"
                target.mkdir()
                (target / "old-artifact").touch()
                neighbor = root / "unrelated"
                neighbor.mkdir()
                (neighbor / "keep").touch()
                if name == "symlink":
                    (target / "old-artifact").unlink()
                    target.rmdir()
                    target.symlink_to(neighbor, target_is_directory=True)
                github_env = root / "github-env"
                environment = {**os.environ, "GITHUB_WORKSPACE": str(root),
                               "ASTRA_CI_RUNNER_ENVIRONMENT": "self-hosted",
                               "ASTRA_CI_TARGET_DIR": str(target), "GITHUB_ENV": str(github_env),
                               "MOCK_TARGET_SIZE": str(size_kib), "MOCK_LOW_DISK": str(int(low_disk)),
                               "MOCK_SCENARIO": name}
                # Real rm acts only on the disposable fixture. Disk and size
                # readings simulate thresholds without filling the test host.
                stubs = '''
df() {
  if [[ "${MOCK_SCENARIO}" == failed-recheck && ! -e "${ASTRA_CI_TARGET_DIR}" ]]; then return 2; fi
  local available=100000000
  if [[ "${MOCK_SCENARIO}" == still-low ]] ||
     [[ "${MOCK_LOW_DISK}" == 1 && ( -e "${ASTRA_CI_TARGET_DIR}" || -L "${ASTRA_CI_TARGET_DIR}" ) ]]; then
    available=10000000
  fi
  printf 'Filesystem Blocks Used Available Capacity Mount\\nfixture 100000000 0 %s 0%% /\\n' "$available"
}
du() { printf '%s\\t%s\\n' "$MOCK_TARGET_SIZE" "$ASTRA_CI_TARGET_DIR"; }
rm() {
  if [[ "${MOCK_SCENARIO}" == failed-cleanup ]]; then return 7; fi
  command rm "$@"
}
'''
                result = subprocess.run(["bash", "-c", stubs + rust_setup_disk_guard()],
                                        env=environment, capture_output=True, text=True)
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual(not target.exists() and not target.is_symlink(), reclaimed)
                self.assertTrue((neighbor / "keep").exists())
                if status == 0:
                    self.assertEqual(github_env.read_text().strip(), f"CARGO_TARGET_DIR={target}")
                else:
                    self.assertFalse(github_env.exists(), "Failed setup must not select retained artifacts")

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
        self.assertEqual(
            args,
            shlex.split(
                "cargo nextest run --locked -p astra-cli --lib --bins "
                "--test cli_pipe --test process_supervisor "
                "--test shell_execution_boundaries --test cli_mcp_setup --profile ci"
            ),
            "Preserve the complete selected offline CLI inventory without test filters",
        )
        self.assertLess(producer.index("Prebuild mock MCP server"), producer.index(test_command))

    def test_vendored_terminal_units_are_locked_and_pty_tests_run_both_readers(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        lanes = (ROOT / "scripts/ci/run-macos-lanes.sh").read_text()
        self.assertIn("run-macos-lanes.sh vendored", workflow)
        for features in ("event-stream", "event-stream,use-dev-tty"):
            self.assertIn(f"cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib --features {features}\n", lanes)
        lock = tomllib.loads((ROOT / "vendor/crossterm/Cargo.lock").read_text())
        self.assertTrue(any(p["name"] == "crossterm" for p in lock["package"]))
        self.assertNotIn("/Cargo.lock", (ROOT / "vendor/crossterm/.gitignore").read_text().splitlines())
        self.assertEqual(workflow.count("segment: [default-reader, dev-tty-reader, reflow]"), 2)
        for features in ("", "--features crossterm/use-dev-tty "):
            self.assertIn(f"cargo test --locked -p astra-cli --lib {features}tui::terminal_startup -- --test-threads=2", lanes)

    def test_mac_native_bundle_removes_extra_graphs_and_node_setup(self):
        workflow = (ROOT / ".github/workflows/test.yml").read_text()
        hosted = workflow.split("\n  macos-hosted:\n", 1)[1].split("\n  vendored-terminal-macos:", 1)[0]
        worker = (ROOT / ".github/workflows/macos-mini.yml").read_text()
        self.assertIn("timeout-minutes: 65", hosted)
        for job in (hosted, worker):
            self.assertNotIn("actions/setup-node", job)
            self.assertIn("run-macos-lanes.sh all", job)
        commands = (ROOT / "scripts/ci/run-macos-lanes.sh").read_text()
        native = commands.split('if [[ "$(uname -s)" == Darwin ]]; then', 1)[1].split("    fi", 1)[0]
        for lane in ("default-reader", "coordination", "native-mio"):
            self.assertIn(f'bash "$0" {lane}', native)
        for lane in ("reflow", "dev-tty-reader", "vendored"):
            self.assertNotIn(f'bash "$0" {lane}\n', native)

    def test_terminal_reflow_uses_real_binary_and_locked_emulator(self):
        commands = (ROOT / "scripts/ci/run-macos-lanes.sh").read_text()
        self.assertIn("cargo build --locked -p astra-cli --bin astra", commands)
        self.assertIn("npm ci --ignore-scripts --prefix scripts/tui-reflow", commands)
        self.assertIn('ASTRA_TEST_BINARY="${CARGO_TARGET_DIR:-$PWD/target}/debug/astra" npm test', commands)

    def test_macos_cli_bash_reuses_default_reader_without_losing_coordination_contracts(self):
        commands = (ROOT / "scripts/ci/run-macos-lanes.sh").read_text()
        reader = commands.split("  default-reader)\n", 1)[1].split("    ;;", 1)[0]
        self.assertLess(reader.index("tui::terminal_startup"), reader.index("observe_git"))
        self.assertLess(reader.index("observe_git"), reader.index("bash_echo_returns_output"))
        self.assertEqual(reader.count("cargo test"), 2)
        contracts = commands.split("  coordination)\n", 1)[1].split("    ;;", 1)[0]
        self.assertIn("cargo clippy --locked --manifest-path Cargo.toml -p astra-tools --all-targets -- -D warnings", contracts)
        self.assertIn("workspace_observation::tests:: -- --test-threads=1", contracts)
        self.assertIn("observe_git", contracts)
        self.assertNotIn("-p astra-cli", contracts)
        self.assertIn('"/usr/bin/git"', commands)
        self.assertIn("timeout=60", commands)

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
