#!/usr/bin/env python3
"""Execute release shell entrypoints with build/network commands stubbed out."""

import os
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[2]


def workflow_run_script(path, step_name):
    """Extract one literal Bash run block from a workflow."""
    workflow = (ROOT / path).read_text()
    block = workflow.split(f"      - name: {step_name}\n", 1)[1]
    block = block.split("        run: |\n", 1)[1]
    lines = []
    for line in block.splitlines():
        if line and len(line) - len(line.lstrip()) < 10:
            break
        lines.append(line[10:] if line else "")
    return "\n".join(lines)


class ReleaseShellTests(unittest.TestCase):
    def test_musl_install_isolates_sources_and_preserves_failures(self):
        script = workflow_run_script(
            ".github/workflows/release-binaries.yml", "Install musl build dependencies"
        )
        scenarios = (
            ("deb822", "modern", "", 0, 2),
            ("legacy", "legacy", "", 0, 2),
            ("empty-deb822-fallback", "empty-modern", "", 0, 2),
            ("missing-sources", "missing", "", 1, 0),
            ("ubuntu-update-failure", "modern", "update", 100, 1),
            ("package-install-failure", "modern", "install", 100, 2),
        )
        for name, source_layout, failed_operation, exit_code, calls in scenarios:
            with self.subTest(scenario=name), tempfile.TemporaryDirectory() as directory:
                fixture = Path(directory)
                apt = fixture / "apt"
                (apt / "sources.list.d").mkdir(parents=True)
                modern = apt / "sources.list.d/ubuntu.sources"
                legacy = apt / "sources.list"
                if source_layout == "modern":
                    modern.write_text("Ubuntu source fixture\n")
                elif source_layout in ("legacy", "empty-modern"):
                    legacy.write_text("Ubuntu source fixture\n")
                    if source_layout == "empty-modern":
                        modern.touch()
                selected = modern if source_layout == "modern" else legacy
                log = fixture / "apt.log"
                # Execute the actual workflow shell, substituting only the fixture
                # filesystem. An unrestricted refresh models a broken Chrome index.
                stub = r'''
sudo() {
    printf '%s\n' "$*" >> "$APT_TEST_LOG"
    case " $* " in
        *" -o Dir::Etc::sourceparts=- "*) ;;
        *) echo 'Chrome index: Hash Sum mismatch' >&2; return 100 ;;
    esac
    if [[ -n "$APT_TEST_FAIL" && " $* " == *" $APT_TEST_FAIL "* ]]; then
        echo 'Ubuntu update or package verification failed' >&2
        return 100
    fi
}
'''
                result = subprocess.run(
                    ["bash", "-c", stub + script.replace("/etc/apt", str(apt))],
                    env={**os.environ, "APT_TEST_LOG": str(log),
                         "APT_TEST_FAIL": failed_operation},
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, exit_code, result.stderr)
                recorded = log.read_text().splitlines() if log.exists() else []
                prefix = (
                    f"apt-get -o Dir::Etc::sourcelist={selected} "
                    "-o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0 "
                    "-o APT::Update::Error-Mode=any "
                )
                self.assertEqual(recorded, [
                    prefix + "update",
                    prefix + "install -y --no-install-recommends musl-tools",
                ][:calls])

    def test_draft_reads_use_publication_credentials(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("contents: write", workflow.split("\n  publish:\n", 1)[1])
        for name in (
            "Detect existing GitHub Release",
            "Prepare canonical GitHub Release body",
            "Resolve staged GitHub Release ID",
            "Verify canonical staged GitHub Release body",
            "Verify exact staged GitHub Release assets",
        ):
            with self.subTest(step=name):
                block = workflow.split(f"      - name: {name}\n", 1)[1]
                block = block.split("      - ", 1)[0]
                self.assertIn("GH_TOKEN: ${{ github.token }}", block)

    def test_draft_detection_and_body_reuse_with_restricted_visibility(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            fake_bin = fixture / "bin"
            fake_bin.mkdir()
            draft = fixture / "draft.json"
            gh = fake_bin / "gh"
            gh.write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
draft = Path(os.environ["ASTRA_TEST_DRAFT"])
visible = os.environ["GH_TOKEN"] == "publication-write" and draft.exists()
if "releases?" in sys.argv[2]:
    if visible:
        print("v0.2.2\\ttrue\\t42")
elif visible:
    print(draft.read_text())
else:
    sys.exit(1)
''', encoding="utf-8")
            gh.chmod(0o755)
            env = {
                **os.environ, "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_TEST_DRAFT": str(draft), "GITHUB_REPOSITORY": "matrixorigin/Astra",
                "SOURCE_TAG": "v0.2.2", "SOURCE_SHA": "a" * 40,
                "GITHUB_RUN_ID": "123", "ORIGINAL_OWNER_RUN_ID": "123",
                "RECOVER_EXISTING_TAG": "false", "SAME_RUN": "false",
                "RUNNER_TEMP": str(fixture), "GITHUB_OUTPUT": "/dev/stdout",
            }

            def run_step(name, **overrides):
                block = workflow.split(f"      - name: {name}\n", 1)[1].split("      - ", 1)[0]
                token = "publication-write" if "GH_TOKEN: ${{ github.token }}" in block and "contents: write" in workflow.split("\n  publish:\n", 1)[1] else "builtin-read"
                result = subprocess.run(
                    ["bash", "-c", workflow_run_script(".github/workflows/release.yml", name)],
                    cwd=ROOT, env={**env, "GH_TOKEN": token, **overrides},
                    capture_output=True, text=True,
                )
                return result

            first = run_step("Detect existing GitHub Release")
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertIn("state=none", first.stdout)
            prepared = run_step("Prepare canonical GitHub Release body", EXISTING_RELEASE_ID="")
            self.assertEqual(prepared.returncode, 0, prepared.stderr)
            self.assertIn("generate_notes=true", prepared.stdout)
            body_path = fixture / "release-body.md"
            body = body_path.read_text() + "Canonical generated notes\n"
            draft.write_text(json.dumps({"draft": True, "tag_name": "v0.2.2", "body": body}))
            for _ in range(2):
                detected = run_step("Detect existing GitHub Release", SAME_RUN="true")
                self.assertEqual(detected.returncode, 0, detected.stderr)
                self.assertIn("state=draft", detected.stdout)
                self.assertIn("release_id=42", detected.stdout)
                reused = run_step("Prepare canonical GitHub Release body", EXISTING_RELEASE_ID="42")
                self.assertEqual(reused.returncode, 0, reused.stderr)
                self.assertIn("generate_notes=false", reused.stdout)
                self.assertEqual(body_path.read_text(), body)
                verified = run_step("Verify canonical staged GitHub Release body",
                                    RELEASE_ID="42", OWNER_RUN_ID="123",
                                    BODY_PATH=str(body_path), GENERATED_NOTES="false")
                self.assertEqual(verified.returncode, 0, verified.stderr)
            conflict = run_step("Detect existing GitHub Release")
            self.assertNotEqual(conflict.returncode, 0)
            self.assertIn("already exists", conflict.stderr)

    def run_idc_settings(self, **overrides):
        script = workflow_run_script(
            ".github/workflows/build_push_to_idc.yml",
            "Resolve IDC target and immutable build identity",
        )
        with tempfile.TemporaryDirectory() as temporary_directory:
            temporary = Path(temporary_directory)
            origin = temporary / "origin.git"
            checkout = temporary / "checkout"
            subprocess.run(["git", "init", "--bare", str(origin)], check=True,
                           capture_output=True)
            subprocess.run(["git", "init", "-b", "main", str(checkout)], check=True,
                           capture_output=True)
            for key, value in (("user.name", "Release Test"),
                               ("user.email", "release-test@example.invalid")):
                subprocess.run(["git", "-C", str(checkout), "config", key, value], check=True)
            marker = checkout / "marker"
            marker.write_text("base\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(checkout), "add", "marker"], check=True)
            subprocess.run(["git", "-C", str(checkout), "commit", "-m", "base"],
                           check=True, capture_output=True)
            base_sha = subprocess.check_output(
                ["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
            subprocess.run(["git", "-C", str(checkout), "remote", "add", "origin", str(origin)],
                           check=True)
            subprocess.run(["git", "-C", str(checkout), "push", "origin", "main"],
                           check=True, capture_output=True)
            subprocess.run(["git", "-C", str(checkout), "switch", "-c", "moi-dev"],
                           check=True, capture_output=True)
            marker.write_text("moi-dev\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(checkout), "commit", "-am", "moi-dev"],
                           check=True, capture_output=True)
            moi_dev_sha = subprocess.check_output(
                ["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
            subprocess.run(["git", "-C", str(checkout), "push", "origin", "moi-dev"],
                           check=True, capture_output=True)
            subprocess.run(["git", "-C", str(checkout), "switch", "main"],
                           check=True, capture_output=True)
            main_sha = subprocess.check_output(
                ["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
            source_ref = overrides.pop("SOURCE_REF", "main")
            if source_ref == "__base_sha__":
                source_ref = base_sha
            env = {
                **os.environ,
                "ARCHITECTURE": "amd64",
                "DEFAULT_BRANCH": "main",
                "SOURCE_REF": source_ref,
                "IDC_REGISTRY": "registry.example:5000",
                "IDC_IMAGE": "registry.example:5000/team/astra",
                "GITHUB_REF": "refs/heads/main",
                "GITHUB_SHA": main_sha,
                "GITHUB_RUN_ID": "123",
                "GITHUB_OUTPUT": "/dev/stdout",
                **overrides,
            }
            result = subprocess.run(["bash", "-c", script], env=env, cwd=checkout,
                                    capture_output=True, text=True)
            return result, {"main": main_sha, "moi-dev": moi_dev_sha, "base": base_sha}

    def test_idc_build_identity(self):
        result, revisions = self.run_idc_settings()
        self.assertEqual(result.returncode, 0, result.stderr)
        outputs = dict(line.split("=", 1) for line in result.stdout.splitlines())
        self.assertEqual(outputs["controller_sha"], revisions["main"])
        self.assertEqual(outputs["source_sha"], revisions["main"])
        self.assertEqual(outputs["source_ref"], "main")
        self.assertEqual(outputs["candidate_image"], "registry.example:5000/team/astra-candidates")
        self.assertEqual(outputs["target_image"], "registry.example:5000/team/astra")
        self.assertRegex(outputs["image_version"], r"^idc-\d{8}T\d{6}Z-" + revisions["main"] + r"-123-amd64$")

    def test_idc_reuses_release_candidate_topology_and_publishes_only_to_idc(self):
        workflow = (ROOT / ".github/workflows/build_push_to_idc.yml").read_text()
        candidates = (ROOT / ".github/workflows/idc-container-candidates.yml").read_text()
        self.assertIn("uses: ./.github/workflows/idc-container-candidates.yml", workflow)
        self.assertIn("Assemble verified IDC manifest", workflow)
        self.assertIn("scripts/copy-immutable-container-tag.sh", workflow)
        self.assertIn("environment: idc-publication", workflow)
        self.assertIn("push-by-digest=true", candidates)
        self.assertIn("make stack-verify", candidates)
        self.assertIn("ref: ${{ inputs.controller_sha }}", candidates)
        self.assertIn("context: source", candidates)
        self.assertIn("file: source/Dockerfile", candidates)
        self.assertIn("ubuntu-24.04-arm", workflow)
        self.assertNotIn("matrixorigin/astra", candidates.split("org.opencontainers.image.source", 1)[0])
        self.assertNotIn("DOCKERHUB_", workflow + candidates)

    def test_idc_immutable_copy_distinguishes_absence_from_lookup_failures(self):
        script = ROOT / "scripts/copy-immutable-container-tag.sh"
        source_digest = "sha256:" + "a" * 64
        conflicting_digest = "sha256:" + "b" * 64

        class HarborHandler(BaseHTTPRequestHandler):
            state = "missing"
            paths = []
            expected_path = "/api/v2.0/projects/team/repositories/astra/artifacts/release"

            def log_message(self, _format, *_args):
                pass

            def do_GET(self):
                type(self).paths.append(self.path)
                if self.path != type(self).expected_path:
                    self.send_response(404)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(json.dumps({"errors": [{"code": "NOT_FOUND"}]}).encode())
                    return
                status = {
                    "missing": 404,
                    "new_repository": 404,
                    "unauthorized": 401,
                    "forbidden": 403,
                    "server_error": 503,
                }.get(type(self).state, 200)
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                if type(self).state == "malformed_not_found":
                    self.wfile.write(b"not a Harbor error envelope")
                    return
                if status == 200:
                    digest = (source_digest if type(self).state == "same"
                              else conflicting_digest)
                    self.wfile.write(json.dumps({"digest": digest}).encode())
                else:
                    code = "NOT_FOUND" if status == 404 else "TEST"
                    self.wfile.write(json.dumps({"errors": [{"code": code}]}).encode())

        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            fake_bin = fixture / "bin"
            fake_bin.mkdir()
            calls = fixture / "calls"
            crane = fake_bin / "crane"
            crane.write_text(
                '''#!/bin/sh
set -eu
printf '%s\\n' "$*" >> "${ASTRA_TEST_CALLS}"
case "$1 $2" in
  "digest source.example/astra:staged")
    printf '%s\\n' "${ASTRA_TEST_SOURCE_DIGEST}"
    ;;
  digest\ *)
    if [ -e "${ASTRA_TEST_STATE_DIR}/copied" ]; then
      printf '%s\\n' "${ASTRA_TEST_SOURCE_DIGEST}"
    else
      exit 92
    fi
    ;;
  "copy --platform=all")
    case "${ASTRA_TEST_TARGET_STATE}" in
      missing|new_repository) ;;
      *) exit 93 ;;
    esac
    touch "${ASTRA_TEST_STATE_DIR}/copied"
    ;;
  *) exit 91 ;;
esac
''',
                encoding="utf-8",
            )
            crane.chmod(0o755)
            common_env = {
                **os.environ,
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_TEST_CALLS": str(calls),
                "ASTRA_TEST_STATE_DIR": str(fixture),
                "ASTRA_TEST_SOURCE_DIGEST": source_digest,
                "ASTRA_TEST_CONFLICTING_DIGEST": conflicting_digest,
                "IDC_REGISTRY_USERNAME": "release-user",
                "IDC_REGISTRY_PASSWORD": "release-password",
                "RUNNER_TEMP": str(fixture),
            }

            def run(state, repository="team/astra"):
                calls.write_text("", encoding="utf-8")
                HarborHandler.state = state
                HarborHandler.paths = []
                repository_name = repository.partition("/")[2]
                encoded_repository_name = quote(quote(repository_name, safe=""), safe="")
                HarborHandler.expected_path = (
                    "/api/v2.0/projects/team/repositories/"
                    f"{encoded_repository_name}/artifacts/release"
                )
                result = subprocess.run(
                    [str(script), "source.example/astra:staged",
                     f"127.0.0.1:{server.server_port}/{repository}:release",
                     f"http://127.0.0.1:{server.server_port}"],
                    env={**common_env, "ASTRA_TEST_TARGET_STATE": state},
                    capture_output=True,
                    text=True,
                )
                return result, calls.read_text(encoding="utf-8"), HarborHandler.paths

            server = ThreadingHTTPServer(("127.0.0.1", 0), HarborHandler)
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)

            for state in ("missing", "new_repository"):
                with self.subTest(state=state):
                    published, published_calls, paths = run(state)
                    self.assertEqual(published.returncode, 0, published.stderr)
                    self.assertIn("copy --platform=all --jobs 2", published_calls)
                    self.assertEqual(
                        paths,
                        ["/api/v2.0/projects/team/repositories/astra/artifacts/release"],
                    )
                    (fixture / "copied").unlink()

            same, same_calls, _ = run("same")
            self.assertEqual(same.returncode, 0, same.stderr)
            self.assertNotIn("copy ", same_calls)

            conflict, conflict_calls, _ = run("conflict")
            self.assertNotEqual(conflict.returncode, 0)
            self.assertIn("already exists with digest", conflict.stderr)
            self.assertNotIn("copy ", conflict_calls)

            nested_conflict, nested_conflict_calls, nested_paths = run(
                "conflict", "team/nested/astra"
            )
            self.assertNotEqual(nested_conflict.returncode, 0)
            self.assertIn("already exists with digest", nested_conflict.stderr)
            self.assertNotIn("copy ", nested_conflict_calls)
            self.assertEqual(
                nested_paths,
                [
                    "/api/v2.0/projects/team/repositories/"
                    "nested%252Fastra/artifacts/release"
                ],
            )

            for state in ("unauthorized", "forbidden", "server_error"):
                with self.subTest(state=state):
                    failed, failed_calls, _ = run(state)
                    self.assertNotEqual(failed.returncode, 0)
                    self.assertIn("could not safely inspect", failed.stderr)
                    self.assertNotIn("copy ", failed_calls)

            HarborHandler.state = "malformed_not_found"
            malformed, malformed_calls, _ = run("malformed_not_found")
            self.assertNotEqual(malformed.returncode, 0)
            self.assertNotIn("copy ", malformed_calls)

            unreachable = ThreadingHTTPServer(("127.0.0.1", 0), HarborHandler)
            unreachable_port = unreachable.server_port
            unreachable.server_close()
            calls.write_text("", encoding="utf-8")
            network_failure = subprocess.run(
                [str(script), "source.example/astra:staged",
                 f"127.0.0.1:{unreachable_port}/team/astra:release",
                 f"http://127.0.0.1:{unreachable_port}"],
                env={**common_env, "ASTRA_TEST_TARGET_STATE": "network_failure"},
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(network_failure.returncode, 0)
            self.assertNotIn("copy ", calls.read_text(encoding="utf-8"))

    def test_idc_resolves_moi_dev_and_allowed_historical_commit(self):
        result, revisions = self.run_idc_settings(SOURCE_REF="moi-dev")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("source_sha=" + revisions["moi-dev"], result.stdout)
        self.assertIn("source_ref=moi-dev", result.stdout)
        result, revisions = self.run_idc_settings(SOURCE_REF="__base_sha__")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("source_sha=" + revisions["base"], result.stdout)
        self.assertIn("source_ref=" + revisions["base"], result.stdout)

    def test_idc_registry_credentials_are_required_before_build(self):
        for workflow in ("build_push_to_idc.yml", "idc-container-candidates.yml"):
            script = workflow_run_script(
                f".github/workflows/{workflow}", "Require IDC registry credentials"
            )
            for missing in (None, "IDC_REGISTRY_USERNAME", "IDC_REGISTRY_PASSWORD"):
                with self.subTest(workflow=workflow, missing=missing):
                    env = {
                        **os.environ,
                        "IDC_REGISTRY_USERNAME": "release-user",
                        "IDC_REGISTRY_PASSWORD": "release-password",
                    }
                    if missing:
                        env[missing] = ""
                    result = subprocess.run(
                        ["bash", "-c", script], env=env, capture_output=True, text=True
                    )
                    if missing:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("Missing required IDC credential: " + missing, result.stdout)
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertNotIn("release-password", result.stdout + result.stderr)

    def test_idc_environment_secrets_are_mapped_across_workflow_call(self):
        caller = (ROOT / ".github/workflows/build_push_to_idc.yml").read_text()
        reusable = (ROOT / ".github/workflows/idc-container-candidates.yml").read_text()
        call = caller.split("\n  candidates:\n", 1)[1].split("\n  stage:\n", 1)[0]
        declaration = reusable.split("\nenv:\n", 1)[0]
        names = {"IDC_REGISTRY_USERNAME", "IDC_REGISTRY_PASSWORD"}
        mapped = set(re.findall(r"^      (IDC_REGISTRY_\w+):", call, re.MULTILINE))
        self.assertEqual(mapped, names)
        self.assertNotIn("secrets: inherit", call)
        for name in names:
            # A name must be mapped as well as declared for GitHub to inject an
            # Environment-only secret. A declaration alone still resolves empty.
            self.assertIn(f"{name}: ${{{{ secrets.{name} }}}}", call)
            self.assertIn(f"      {name}:\n        required: false", declaration)
        for job in ("build", "smoke"):
            block = reusable.split(f"\n  {job}:\n", 1)[1].split("\n  smoke:\n", 1)[0]
            self.assertIn("    environment: idc-publication\n", block)

    def test_idc_rejects_non_main_controller(self):
        result, _ = self.run_idc_settings(GITHUB_REF="refs/heads/moi-dev")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Run this workflow from main", result.stdout)

    def test_idc_rejects_arbitrary_source_ref(self):
        result, _ = self.run_idc_settings(SOURCE_REF="feature/test")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source_ref must be main, moi-dev, or a full commit SHA", result.stdout)

    def test_idc_missing_configuration_stops_before_build(self):
        for key in ("IDC_REGISTRY", "IDC_IMAGE"):
            with self.subTest(key=key):
                result, _ = self.run_idc_settings(**{key: ""})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Missing required IDC configuration: " + key, result.stdout)
                self.assertNotIn("image_version=", result.stdout)

    def test_idc_rejects_wrong_or_tagged_repository(self):
        for image in ("docker.io/team/astra", "registry.example:5000/",
                      "registry.example:5000/team/astra:latest",
                      "registry.example:5000/team/astra@sha256:abc",
                      "registry.example:5000/team/astra bad"):
            with self.subTest(image=image):
                result, _ = self.run_idc_settings(IDC_IMAGE=image)
                self.assertNotEqual(result.returncode, 0)
        result, _ = self.run_idc_settings(IDC_REGISTRY="https://registry.example")
        self.assertNotEqual(result.returncode, 0)

    def test_idc_architecture_matrix(self):
        result, _ = self.run_idc_settings(ARCHITECTURE="all")
        self.assertEqual(result.returncode, 0, result.stderr)
        outputs = dict(line.split("=", 1) for line in result.stdout.splitlines())
        self.assertIn('"platform":"linux/amd64"', outputs["matrix"])
        self.assertIn('"platform":"linux/arm64"', outputs["matrix"])
        self.assertNotRegex(outputs["image_version"], r"-amd64$")
        result, _ = self.run_idc_settings(ARCHITECTURE="s390x")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unsupported architecture: s390x", result.stdout)

    def test_client_arguments_with_and_without_features(self):
        script = workflow_run_script(
            ".github/workflows/release-binaries.yml", "Build client candidates"
        )
        script = script.replace("${{ matrix.target }}", "test-target")
        # POSIX positional parameters also work on macOS's Bash 3.2.
        # Run that portion under sh as well as bash to guard portability.
        for shell in ("bash", "sh"):
            for features in ("", "astra-cli/release-vendored-openssl"):
                with self.subTest(shell=shell, features=features):
                    body = script if shell == "bash" else script.replace("set -euo pipefail", "set -eu")
                    result = subprocess.run(
                        [shell, "-c", 'cargo() { printf "%s\\n" "$@"; };\n' + body],
                        env={**os.environ, "RELEASE_FEATURES": features},
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    expected = ["build", "--release", "--locked", "--no-default-features"]
                    if features:
                        expected += ["--features", features]
                    expected += ["--manifest-path", "Cargo.toml", "--target", "test-target",
                                 "-p", "astra-cli", "--bin", "astra",
                                 "-p", "astra-edge", "--bin", "astra-edge"]
                    self.assertEqual(result.stdout.splitlines(), expected)

    def test_release_tag_creation_requires_default_branch_ancestry(self):
        script = ROOT / "scripts/reconcile-release-tag.sh"
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            remote = fixture / "remote"
            checkout = fixture / "checkout"

            def git(*args, cwd=remote):
                return subprocess.run(
                    ["git", *args], cwd=cwd, check=True,
                    capture_output=True, text=True,
                ).stdout.strip()

            remote.mkdir()
            git("init", "--initial-branch=main")
            git("config", "user.name", "Release Test")
            git("config", "user.email", "release-test@example.invalid")
            git("-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "source")
            source_sha = git("rev-parse", "HEAD")
            git("clone", str(remote), str(checkout), cwd=fixture)
            fake_bin = fixture / "bin"
            fake_bin.mkdir()
            calls = fixture / "calls"
            (fake_bin / "gh").write_text(
                """#!/bin/sh
set -eu
printf '%s\\n' "gh $*" >> "${ASTRA_TEST_CALLS}"
case "$*" in
  "api --method POST repos/matrixorigin/Astra/git/tags "*)
    printf '%s\\n' 'owned-tag-object'
    ;;
  "api --method POST repos/matrixorigin/Astra/git/refs "*) exit 0 ;;
  "api repos/matrixorigin/Astra/git/tags/owned-tag-object")
    printf '{"object":{"sha":"%s"},"message":"Astra v0.2.2\\\\n\\\\nRelease-Run: https://github.com/matrixorigin/Astra/actions/runs/123"}\\n' \
      "${SOURCE_SHA}"
    ;;
  *) exit 2 ;;
esac
""",
                encoding="utf-8",
            )
            (fake_bin / "gh").chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_TEST_CALLS": str(calls),
                "SOURCE_SHA": source_sha,
                "GITHUB_SERVER_URL": "https://github.com",
            }

            def create(selected_source=source_sha):
                calls.write_text("")
                return subprocess.run(
                    [str(script), "create", "matrixorigin/Astra", "v0.2.2",
                     selected_source, "123", "main", ""],
                    cwd=checkout, env=env, capture_output=True, text=True,
                )

            for advanced in (False, True):
                with self.subTest(advanced=advanced):
                    if advanced:
                        git("-c", "commit.gpgsign=false", "commit", "--allow-empty",
                            "-m", "main advances while candidates build")
                    result = create()
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "owned-tag-object\n")
                    recorded = calls.read_text()
                    self.assertIn(f"-f object={source_sha}", recorded)
                    self.assertIn("gh api --method POST repos/matrixorigin/Astra/git/refs", recorded)

            with self.subTest(source="missing"):
                result = create("0" * 40)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("is not present as a commit", result.stderr)
                self.assertEqual(calls.read_text(), "")

            with self.subTest(source="ahead of main"):
                git("checkout", "--detach", "FETCH_HEAD", cwd=checkout)
                git("-c", "user.name=Release Test", "-c",
                    "user.email=release-test@example.invalid", "-c",
                    "commit.gpgsign=false", "commit", "--allow-empty",
                    "-m", "unmerged source", cwd=checkout)
                result = create(git("rev-parse", "HEAD", cwd=checkout))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("is no longer reachable from main", result.stderr)
                self.assertEqual(calls.read_text(), "")

            with self.subTest(history="shallow"):
                shallow_file = checkout / ".git" / "shallow"
                shallow_file.write_text(source_sha + "\n")
                try:
                    result = create()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("requires a complete checkout", result.stderr)
                    self.assertEqual(calls.read_text(), "")
                finally:
                    shallow_file.unlink()

            git("checkout", "--orphan", "replacement")
            git("-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "unrelated history")
            git("branch", "-f", "main", "HEAD")
            result = create()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("is no longer reachable from main", result.stderr)
            self.assertEqual(calls.read_text(), "")

            git("branch", "-D", "main")
            result = create()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Could not fetch", result.stderr)
            self.assertEqual(calls.read_text(), "")

    def test_release_tag_creation_is_idempotent_for_the_same_run(self):
        script = ROOT / "scripts/reconcile-release-tag.sh"
        with tempfile.TemporaryDirectory() as directory:
            fixture = Path(directory)
            fake_bin = fixture / "bin"
            fake_bin.mkdir()
            calls = fixture / "calls"
            (fake_bin / "git").write_text(
                """#!/bin/sh
set -eu
printf '%s\\n' "git $*" >> "${ASTRA_TEST_CALLS}"
case "$*" in
  "ls-remote origin refs/tags/v0.2.2")
    printf '%s\\trefs/tags/v0.2.2\\n' 'owned-tag-object'
    ;;
  *) exit 2 ;;
esac
""",
                encoding="utf-8",
            )
            (fake_bin / "gh").write_text(
                """#!/bin/sh
set -eu
printf '%s\\n' "gh $*" >> "${ASTRA_TEST_CALLS}"
case "$*" in
  *"--method POST"*) exit 99 ;;
esac
printf '{"object":{"sha":"%s"},"message":"Astra v0.2.2\\\\n\\\\nRelease-Run: https://github.com/matrixorigin/Astra/actions/runs/123"}\\n' \
  "${SOURCE_SHA}"
""",
                encoding="utf-8",
            )
            (fake_bin / "git").chmod(0o755)
            (fake_bin / "gh").chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_TEST_CALLS": str(calls),
                "SOURCE_SHA": "verified-source-sha",
                "GITHUB_SERVER_URL": "https://github.com",
            }
            result = subprocess.run(
                [
                    str(script),
                    "create",
                    "matrixorigin/Astra",
                    "v0.2.2",
                    "verified-source-sha",
                    "123",
                    "main",
                    "",
                ],
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "owned-tag-object\n")
            self.assertNotIn("--method POST", calls.read_text(encoding="utf-8"))

    def test_docker_optional_mirrors_unset_and_empty(self):
        dockerfile = (ROOT / "Dockerfile").read_text().replace("\\\n", "")
        commands = re.findall(r"^RUN (set -eux;.*)$", dockerfile, re.MULTILINE)
        commands = [command for command in commands
                    if "CARGO_REGISTRY" in command or "DEBIAN_MIRROR" in command]
        self.assertEqual(len(commands), 3)
        stubs = '\n'.join(f'{name}() {{ :; }}' for name in
                          ("apt_get", "rm", "groupadd", "useradd"))
        for empty in (False, True):
            env = {key: value for key, value in os.environ.items()
                   if key not in ("CARGO_REGISTRY", "DEBIAN_MIRROR")}
            if empty:
                env.update(CARGO_REGISTRY="", DEBIAN_MIRROR="")
            for command in commands:
                with self.subTest(empty=empty, command=command[:70]):
                    result = subprocess.run(
                        ["sh", "-c", stubs + '\n' + command.replace("apt-get", "apt_get")], env=env,
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
