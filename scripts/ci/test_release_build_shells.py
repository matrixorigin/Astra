#!/usr/bin/env python3
"""Execute release shell entrypoints with build/network commands stubbed out."""

import os
import json
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


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
                "DEFAULT_BRANCH": "main",
                "SOURCE_REF": source_ref,
                "IDC_REGISTRY": "registry.example:5000",
                "IDC_IMAGE": "registry.example:5000/team/astra",
                "IDC_RUNNER": "idc-amd64",
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
        self.assertRegex(outputs["image_version"], r"^idc-\d{8}T\d{6}Z-" + revisions["main"] + r"-123-amd64$")

    def test_idc_build_stays_local_until_smoke_succeeds(self):
        workflow = (ROOT / ".github/workflows/build_push_to_idc.yml").read_text()
        build = workflow.index("Build the IDC candidate locally")
        smoke = workflow.index("Verify health and exact memory round trip")
        login = workflow.index("docker/login-action")
        publish = workflow.index('docker push "${target}"')
        self.assertLess(build, smoke)
        self.assertLess(smoke, login)
        self.assertLess(login, publish)
        self.assertIn("environment: idc-publication", workflow)
        self.assertIn("load: true", workflow)
        self.assertIn("push: false", workflow)
        self.assertNotIn("release-container-candidates.yml", workflow)

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
        script = workflow_run_script(
            ".github/workflows/build_push_to_idc.yml",
            "Require IDC registry credentials",
        )
        for missing in ("IDC_REGISTRY_USERNAME", "IDC_REGISTRY_PASSWORD"):
            with self.subTest(missing=missing):
                env = {
                    **os.environ,
                    "IDC_REGISTRY_USERNAME": "release-user",
                    "IDC_REGISTRY_PASSWORD": "release-password",
                    missing: "",
                }
                result = subprocess.run(
                    ["bash", "-c", script], env=env, capture_output=True, text=True
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Missing required IDC credential: " + missing, result.stdout)
                self.assertNotIn("release-password", result.stdout + result.stderr)

    def test_idc_rejects_non_main_controller_and_arbitrary_ref(self):
        result, _ = self.run_idc_settings(GITHUB_REF="refs/heads/moi-dev")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Run this workflow from main", result.stdout)
        result, _ = self.run_idc_settings(SOURCE_REF="feature/test")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("source_ref must be main, moi-dev, or a full commit SHA", result.stdout)

    def test_idc_missing_configuration_stops_before_build(self):
        for key in ("IDC_REGISTRY", "IDC_IMAGE", "IDC_RUNNER"):
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

    def test_release_tag_creation_requires_current_head(self):
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
  "ls-remote origin refs/tags/"*) exit 0 ;;
  "ls-remote origin refs/heads/main")
    printf '%s\\trefs/heads/main\\n' "${ASTRA_TEST_DEFAULT_SHA}"
    ;;
  "fetch --no-tags origin refs/heads/main:refs/remotes/origin/main") exit 0 ;;
  "merge-base --is-ancestor verified-source-sha new-main-sha") exit 0 ;;
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
  "api --method POST repos/matrixorigin/Astra/git/tags "*)
    printf '%s\\n' 'owned-tag-object'
    ;;
  "api --method POST repos/matrixorigin/Astra/git/refs "*) exit 0 ;;
  "api repos/matrixorigin/Astra/git/tags/owned-tag-object")
    printf '{"object":{"sha":"%s"},"message":"Astra v0.2.2\\\\n\\\\nRelease-Run: https://github.com/matrixorigin/Astra/actions/runs/123"}\\n' \
      'verified-source-sha'
    ;;
  *) exit 2 ;;
esac
""",
                encoding="utf-8",
            )
            (fake_bin / "git").chmod(0o755)
            (fake_bin / "gh").chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "ASTRA_TEST_CALLS": str(calls),
                "ASTRA_TEST_DEFAULT_SHA": "verified-source-sha",
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
            recorded_calls = calls.read_text(encoding="utf-8")
            self.assertIn("gh api --method POST repos/matrixorigin/Astra/git/tags", recorded_calls)
            self.assertIn("gh api --method POST repos/matrixorigin/Astra/git/refs", recorded_calls)

            calls.write_text("")
            stale = subprocess.run(
                [str(script), "create", "matrixorigin/Astra", "v0.2.2",
                 "verified-source-sha", "123", "main", ""],
                env={**env, "ASTRA_TEST_DEFAULT_SHA": "new-main-sha"},
                capture_output=True, text=True,
            )
            self.assertNotEqual(stale.returncode, 0)
            self.assertIn("Start a new normal release run", stale.stderr)
            self.assertNotIn("gh ", calls.read_text())

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
