#!/usr/bin/env python3
"""Exercise core metadata and public artifact identity with Cargo/Git fixtures."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[2]


class BuildMetadataTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.repo = self.root / "source"
        self.repo.mkdir()
        # Keep local build overrides, Git configuration, and compiler caches out
        # of the fixture. These minimal fixtures build offline; the public-probe
        # regression explicitly fetches its serialization dependencies first.
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("ASTRA_", "GIT_", "CARGO_", "RUSTC_"))
            and key not in ("RUSTFLAGS", "RUSTDOCFLAGS", "RUSTUP_TOOLCHAIN")
        }
        self.env.update({
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "CI Fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "CI Fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
            "CARGO_TARGET_DIR": str(self.repo / "target"),
            "CARGO_INCREMENTAL": "0",
            "CARGO_NET_OFFLINE": "true",
            "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
        })
        (self.repo / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/core", "crates/service", "crates/identity", "crates/app"]\nresolver = "2"\n'
        )
        shutil.copyfile(ROOT / "rust-toolchain.toml", self.repo / "rust-toolchain.toml")
        (self.repo / ".gitignore").write_text("/target/\n")
        for path in ("README.md", "docs/notes.md", "web/app.js"):
            tracked = self.repo / path
            tracked.parent.mkdir(parents=True, exist_ok=True)
            tracked.write_text("fixture\n")
        for name in ("identity", "app"):
            package = self.repo / "crates" / name
            (package / "src").mkdir(parents=True)
            manifest = f'[package]\nname = "fixture_{name}"\nversion = "0.1.0"\nedition = "2024"\n'
            if name == "app":
                manifest += '[dependencies]\nfixture_identity = { path = "../identity" }\n'
            (package / "Cargo.toml").write_text(manifest)
        for name, manifest, source in (
            ("core", '[package]\nname="astra-core"\nversion="0.1.0"\nedition="2024"\n',
             'pub fn value() -> u8 { 1 }\n'),
            ("service", '[package]\nname="fixture_service"\nversion="0.1.0"\nedition="2024"\n[dependencies]\nastra-core={path="../core"}\n',
             'pub fn value() -> u8 { astra_core::value() }\n'),
        ):
            package = self.repo / "crates" / name
            (package / "src").mkdir(parents=True)
            (package / "Cargo.toml").write_text(manifest)
            (package / "src/lib.rs").write_text(source)
        with (self.repo / "crates/identity/Cargo.toml").open("a") as output:
            output.write('[dependencies]\nastra-core={path="../core"}\n')
        with (self.repo / "crates/app/Cargo.toml").open("a") as output:
            output.write('fixture_service={path="../service"}\n')
        shutil.copyfile(ROOT / "crates/astra-build-info/build.rs", self.repo / "crates/identity/build.rs")
        (self.repo / "crates/identity/src/lib.rs").write_text('''
pub fn identity() -> [&'static str; 3] {
    [env!("ASTRA_BUILD_GIT_SHA"), env!("ASTRA_BUILD_GIT_DIRTY"),
     env!("ASTRA_BUILD_ATTESTATION_NONCE")]
}
''')
        (self.repo / "crates/app/src/main.rs").write_text('''
fn main() { assert_eq!(fixture_service::value(), 1); println!("{:?}", fixture_identity::identity()); }
''')
        self.run_command("cargo", "generate-lockfile", "--offline")
        self.git("init", "-b", "main")
        self.git("config", "core.logAllRefUpdates", "false")
        self.git("add", ".")
        self.git("commit", "-m", "fixture")

    def run_command(self, *args):
        result = subprocess.run(args, cwd=self.repo, env=self.env, text=True,
                                capture_output=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def git(self, *args):
        return self.run_command("git", *args).stdout.strip()

    def build(self, *, fresh=False, fresh_core=False, expected=None):
        result = self.run_command("cargo", "run", "--locked", "--offline", "--verbose",
                                  "-p", "fixture_app")
        if fresh or fresh_core:
            self.assertIn("Fresh fixture_identity", result.stderr)
            self.assertNotIn("Compiling fixture_identity", result.stderr)
        if fresh:
            self.assertIn("Fresh fixture_app", result.stderr)
            self.assertNotIn("Compiling fixture_", result.stderr)
        identity = json.loads(result.stdout)
        if expected is not None:
            self.assertEqual(identity, expected)
        return identity

    def test_metadata_commit_preserves_core_and_service_artifacts(self):
        self.build()
        self.git("commit", "--allow-empty", "-m", "metadata-only change")
        result = self.run_command("cargo", "run", "--locked", "--offline", "--verbose", "-p", "fixture_app")
        self.assertEqual(json.loads(result.stdout)[0], self.git("rev-parse", "HEAD"))
        for package in ("astra-core", "fixture_service"):
            self.assertIn(f"Fresh {package}", result.stderr)
            self.assertNotIn(f"Compiling {package}", result.stderr)
        for package in ("fixture_identity", "fixture_app"):
            self.assertIn(f"Compiling {package}", result.stderr)
        self.build(fresh=True)
        core = self.repo / "crates/core/src/lib.rs"
        core.write_text(core.read_text() + "\n// real source edit\n")
        result = self.run_command("cargo", "run", "--locked", "--offline", "--verbose", "-p", "fixture_app")
        for package in ("astra-core", "fixture_service", "fixture_identity", "fixture_app"):
            self.assertIn(f"Compiling {package}", result.stderr)
        self.assertEqual(json.loads(result.stdout)[0], self.git("rev-parse", "HEAD"))

    def test_cold_detached_checkout_reuses_second_and_third_build(self):
        # packed-refs absent reproduces A; present isolates the index refresh B.
        for packed in (False, True):
            with self.subTest(packed=packed):
                source = self.repo
                clone = self.root / f"clone-{packed}"
                self.git("clone", "--no-local", str(source), str(clone))
                self.repo = clone
                self.env["CARGO_TARGET_DIR"] = str(clone / "target")
                try:
                    self.git("checkout", "--detach")
                    if packed:
                        self.git("pack-refs", "--all")
                    else:
                        (clone / ".git/packed-refs").unlink(missing_ok=True)
                    shutil.rmtree(clone / ".git/logs", ignore_errors=True)
                    # Force stale stat data without changing content. Do not run
                    # git status/refresh before Cargo: the build must be read-only
                    # even on the very first checkout build.
                    tracked = clone / "crates/identity/src/lib.rs"
                    stat = tracked.stat()
                    os.utime(tracked, ns=(stat.st_atime_ns, stat.st_mtime_ns - 10_000_000_000))
                    index = clone / ".git/index"
                    before = (index.read_bytes(), index.stat().st_mtime_ns)
                    nonce = "a" * 64
                    self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = nonce
                    expected = [self.git("rev-parse", "HEAD"), "false", nonce]
                    self.build(expected=expected)
                    self.assertTrue(
                        (index.read_bytes(), index.stat().st_mtime_ns) == before,
                        "The first build must not rewrite the checkout's index",
                    )
                    self.build(fresh=True, expected=expected)
                    self.build(fresh=True, expected=expected)
                finally:
                    self.repo = source
                    self.env["CARGO_TARGET_DIR"] = str(source / "target")

    def test_packed_symbolic_ref_and_linked_worktree_follow_new_commits(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                source = self.repo
                if linked:
                    worktree = self.root / "linked"
                    self.git("worktree", "add", "-b", "linked", str(worktree))
                    self.repo = worktree
                    self.env["CARGO_TARGET_DIR"] = str(worktree / "target")
                try:
                    self.git("pack-refs", "--all", "--prune")
                    expected = [self.git("rev-parse", "HEAD"), "true", "absent"]
                    self.build(expected=expected)
                    self.build(fresh=True, expected=expected)
                    # Fetch-like loose remote/tag updates must not invalidate
                    # a packed local branch, including in linked worktrees.
                    remote_sha = self.git("commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "remote")
                    for ref in ("refs/remotes/origin/main", "refs/tags/fixture"):
                        for sha in (expected[0], remote_sha):
                            self.git("update-ref", ref, sha)
                            self.build(fresh=True, expected=expected)
                    # Move only the ref (no checkout/index refresh/reflog). This
                    # creates a loose ref for a previously packed branch.
                    new_sha = self.git("commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "empty")
                    self.git("update-ref", self.git("symbolic-ref", "HEAD"), new_sha)
                    expected[0] = new_sha
                    self.build(expected=expected)
                    self.build(fresh=True, expected=expected)
                    self.git("pack-refs", "--all", "--prune")
                    self.build(expected=expected)
                    self.build(fresh=True, expected=expected)
                finally:
                    self.repo = source
                    self.env["CARGO_TARGET_DIR"] = str(source / "target")

    def test_detached_head_staged_changes_and_nonce_invalidate_identity(self):
        self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = "a" * 64
        self.git("checkout", "--detach")
        sha = self.git("rev-parse", "HEAD")
        self.build(expected=[sha, "false", "a" * 64])
        (self.repo / "tracked.txt").write_text("changed\n")
        self.git("add", "tracked.txt")
        self.build(expected=[sha, "true", "a" * 64])
        self.build(fresh=True, expected=[sha, "true", "a" * 64])
        self.git("commit", "-m", "tracked change")
        sha = self.git("rev-parse", "HEAD")
        self.build(expected=[sha, "false", "a" * 64])
        for nonce in ("a" * 64, "b" * 64, "invalid"):
            self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = nonce
            expected = [sha, "false" if len(nonce) == 64 else "true",
                        nonce if len(nonce) == 64 else "absent"]
            self.build(expected=expected)
            self.build(fresh=True, expected=expected)

    def test_source_archive_keeps_explicit_identity_and_unknown_fallback(self):
        shutil.rmtree(self.repo / ".git")
        for sha, dirty in (("a" * 40, "false"), ("b" * 64, "true"), ("invalid", "invalid")):
            self.env["ASTRA_BUILD_SOURCE_GIT_SHA"] = sha
            self.env["ASTRA_BUILD_SOURCE_GIT_DIRTY"] = dirty
            expected = [sha, dirty, "absent"] if dirty != "invalid" else ["unknown", "true", "absent"]
            self.build(expected=expected)
            self.build(fresh=True, expected=expected)

    def test_incomplete_source_declarations_cannot_claim_clean(self):
        sha = self.git("rev-parse", "HEAD")
        for source_sha, dirty in ((None, "false"), ("invalid", "false"),
                                  (sha, None), (sha, "invalid")):
            with self.subTest(source_sha=source_sha, dirty=dirty):
                for name, value in (("ASTRA_BUILD_SOURCE_GIT_SHA", source_sha),
                                    ("ASTRA_BUILD_SOURCE_GIT_DIRTY", dirty)):
                    if value is None:
                        self.env.pop(name, None)
                    else:
                        self.env[name] = value
                self.build(expected=[sha, "true", "absent"])
                self.build(fresh=True, expected=[sha, "true", "absent"])

    def test_non_attested_builds_do_not_rebuild_for_unrelated_tracked_edits(self):
        paths = [self.repo / name for name in ("README.md", "docs/notes.md", "web/app.js")]
        downstream = self.repo / "crates/app/src/main.rs"
        source = downstream.read_text()
        sha = self.git("rev-parse", "HEAD")
        for nonce in (None, "invalid", "g" * 64):
            with self.subTest(nonce=nonce):
                for path in paths:
                    path.write_text("fixture\n")
                downstream.write_text(source)
                if nonce is not None:
                    self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = nonce
                expected = [sha, "true", "absent"]
                self.build(expected=expected)
                for path in paths:
                    path.touch()
                    self.build(fresh=True, expected=expected)
                    path.write_text("first edit\n")
                    self.build(fresh=True, expected=expected)
                    path.write_text("already dirty, edited again\n")
                    self.build(fresh=True, expected=expected)
                downstream.write_text(source + "\n// downstream-only edit\n")
                self.build(fresh_core=True, expected=expected)

    def test_real_edited_artifact_cannot_pass_clean_probe_after_source_restore(self):
        # Compile the unmodified production JSON entrypoint, not a fake probe or
        # a JSON projection of the fixture's env vars. Seed resolution from the
        # repository lockfile; only this fixture needs serde's small dependency
        # closure. Fetch it before switching back to fully offline builds.
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
        versions = {p["name"]: p["version"] for p in lock["package"]}
        manifest = self.repo / "crates/identity/Cargo.toml"
        with manifest.open("a") as output:
            output.write(f'serde = {{ version = "={versions["serde"]}", features = ["derive"] }}\n')
            output.write(f'serde_json = "={versions["serde_json"]}"\n')
        shutil.copyfile(ROOT / "Cargo.lock", self.repo / "Cargo.lock")
        (self.repo / "crates/core/src/lib.rs").write_text('''
pub fn value() -> u8 { 1 }
pub mod history_work_baseline {
    pub struct BuildAttestation { pub git_sha: &'static str, pub git_dirty: &'static str, pub nonce: &'static str }
    pub fn verify_current_build_attestation(_: &str, _: &str, _: BuildAttestation) -> std::io::Result<String> {
        unreachable!("probe must not initialize baseline verification")
    }
}
''')
        shutil.copyfile(ROOT / "crates/astra-build-info/src/lib.rs",
                        self.repo / "crates/identity/src/build_info.rs")
        (self.repo / "crates/identity/src/lib.rs").write_text('''
pub mod build_info;

''')
        downstream = self.repo / "crates/app/src/main.rs"
        original = '''fn main() {
    if fixture_identity::build_info::write_json_if_requested().unwrap() { return; }
    println!("original behavior");
}
'''
        downstream.write_text(original)
        self.env["CARGO_NET_OFFLINE"] = "false"
        self.run_command("cargo", "fetch")
        resolved = tomllib.loads((self.repo / "Cargo.lock").read_text())
        pinned = {(p["name"], p["version"], p.get("checksum")) for p in lock["package"]}
        for package in resolved["package"]:
            if "source" in package:
                self.assertIn((package["name"], package["version"], package.get("checksum")), pinned)
        self.env["CARGO_NET_OFFLINE"] = "true"
        self.git("add", ".")
        self.git("commit", "-m", "production identity fixture")
        sha = self.git("rev-parse", "HEAD")
        spec = importlib.util.spec_from_file_location(
            "build_metadata_preflight", ROOT / "scripts/harness/preflight.py")
        preflight = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(preflight)
        binary = self.repo / "target/debug/fixture_app"

        def probe():
            return preflight.probe_build_info(
                binary, expected_git_sha=sha, expected_target=None)

        for mode in ("ordinary", "invalid-nonce", "attested", "explicit"):
            with self.subTest(mode=mode):
                self.env.pop("ASTRA_BUILD_ATTESTATION_NONCE", None)
                self.env.pop("ASTRA_BUILD_SOURCE_GIT_SHA", None)
                self.env.pop("ASTRA_BUILD_SOURCE_GIT_DIRTY", None)
                if mode == "attested":
                    self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = "a" * 64
                elif mode == "invalid-nonce":
                    self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = "invalid"
                elif mode == "explicit":
                    self.env["ASTRA_BUILD_SOURCE_GIT_SHA"] = sha
                    self.env["ASTRA_BUILD_SOURCE_GIT_DIRTY"] = "false"
                self.run_command("cargo", "build", "--locked", "--offline", "-p", "fixture_app")
                self.assertEqual(self.run_command(str(binary)).stdout.strip(), "original behavior")
                clean_ok, clean_detail = probe()
                downstream.write_text(original.replace("original behavior", "edited behavior"))
                if mode == "explicit":
                    # The trusted launcher refreshes its declaration each time.
                    self.env["ASTRA_BUILD_SOURCE_GIT_DIRTY"] = "true"
                result = self.run_command("cargo", "build", "--locked", "--offline",
                                          "--verbose", "-p", "fixture_app")
                if mode in ("ordinary", "invalid-nonce"):
                    self.assertIn("Fresh fixture_identity", result.stderr)
                    self.assertNotIn("Compiling fixture_identity", result.stderr)
                self.assertEqual(self.run_command(str(binary)).stdout.strip(), "edited behavior")
                downstream.write_text(original)
                self.assertEqual(self.git("status", "--porcelain=v1", "--untracked-files=no"), "")
                # Do not rebuild: the checkout is clean, but the retained binary
                # demonstrably still executes the edited code.
                self.assertEqual(self.run_command(str(binary)).stdout.strip(), "edited behavior")
                ok, detail = probe()
                self.assertFalse(ok, detail)
                self.assertIn("git_dirty", detail)
                self.assertEqual(clean_ok, mode in ("attested", "explicit"), clean_detail)

    def test_attestation_rechecks_dirty_state_with_the_same_valid_nonce(self):
        sha = self.git("rev-parse", "HEAD")
        self.build(expected=[sha, "true", "absent"])
        readme = self.repo / "README.md"
        readme.write_text("unstaged edit before attestation\n")
        self.build(fresh=True, expected=[sha, "true", "absent"])
        nonce = "a" * 64
        self.env["ASTRA_BUILD_ATTESTATION_NONCE"] = nonce
        self.build(expected=[sha, "true", nonce])
        readme.write_text("fixture\n")
        self.build(expected=[sha, "false", nonce])
        tracked = self.repo / "crates/app/src/main.rs"
        original = tracked.read_text()
        tracked.write_text(original + "\n// local change\n")
        # Reusing an attestation nonce must still detect unstaged downstream
        # edits without git add/status or an index refresh.
        self.build(expected=[sha, "true", nonce])
        self.build(fresh=True, expected=[sha, "true", nonce])
        tracked.write_text(original)
        self.build(expected=[sha, "false", nonce])
        self.build(fresh=True, expected=[sha, "false", nonce])
        tracked = self.repo / ".gitignore"
        original = tracked.read_text()
        tracked.unlink()
        self.build(expected=[sha, "true", nonce])
        tracked.write_text(original)
        self.build(expected=[sha, "false", nonce])
        self.build(fresh=True, expected=[sha, "false", nonce])
        # Leaving attestation mode must remove the broad source watches.
        del self.env["ASTRA_BUILD_ATTESTATION_NONCE"]
        self.build(expected=[sha, "true", "absent"])
        readme.write_text("ordinary development again\n")
        self.build(fresh=True, expected=[sha, "true", "absent"])


if __name__ == "__main__":
    unittest.main()
