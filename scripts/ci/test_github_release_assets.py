#!/usr/bin/env python3
"""Test exact staged GitHub Release asset verification without network access."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
VERIFIER = ROOT / "scripts/verify_github_release_assets.py"


class GitHubReleaseAssetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.fixture = Path(self.temporary.name)
        self.assets = self.fixture / "assets"
        self.fake_bin = self.fixture / "bin"
        self.assets.mkdir()
        self.fake_bin.mkdir()
        (self.assets / "astra.tar.gz").write_bytes(b"astra archive")
        (self.assets / "checksums.txt").write_bytes(b"checksums")
        fake_gh = self.fake_bin / "gh"
        fake_gh.write_text(
            """#!/usr/bin/env python3
import json
import os
import sys

if sys.argv[1:2] != ["api"] or "/releases/123/assets?" not in sys.argv[2]:
    raise SystemExit(90)
if os.environ.get("ASTRA_TEST_GH_FAILURE") == "true":
    print("simulated GitHub failure", file=sys.stderr)
    raise SystemExit(1)
page = int(sys.argv[2].rsplit("page=", 1)[1])
print(os.environ.get("ASTRA_TEST_REMOTE_ASSETS", "[]") if page == 1 else "[]")
""",
            encoding="utf-8",
        )
        fake_gh.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def remote_asset(self, name: str) -> dict[str, object]:
        content = (self.assets / name).read_bytes()
        return {
            "name": name,
            "size": len(content),
            "digest": f"sha256:{hashlib.sha256(content).hexdigest()}",
            "state": "uploaded",
        }

    def run_verifier(
        self, remote_assets: list[dict[str, object]], *, api_failure: bool = False
    ) -> subprocess.CompletedProcess[str]:
        environment = {
            **os.environ,
            "PATH": f"{self.fake_bin}{os.pathsep}{os.environ['PATH']}",
            "ASTRA_TEST_REMOTE_ASSETS": json.dumps(remote_assets),
            "ASTRA_TEST_GH_FAILURE": "true" if api_failure else "false",
        }
        return subprocess.run(
            [str(VERIFIER), "matrixorigin/Astra", "123", str(self.assets)],
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

    def test_accepts_exact_uploaded_asset_set(self) -> None:
        remote = [
            self.remote_asset("astra.tar.gz"),
            self.remote_asset("checksums.txt"),
        ]
        result = self.run_verifier(remote)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_missing_extra_or_changed_assets(self) -> None:
        exact = [
            self.remote_asset("astra.tar.gz"),
            self.remote_asset("checksums.txt"),
        ]
        cases = {
            "missing": exact[:1],
            "extra": [*exact, {**exact[0], "name": "unexpected.bin"}],
            "digest": [{**exact[0], "digest": "sha256:" + "0" * 64}, exact[1]],
            "not-uploaded": [{**exact[0], "state": "new"}, exact[1]],
            "duplicate": [*exact, exact[0]],
        }
        for name, remote in cases.items():
            with self.subTest(name=name):
                result = self.run_verifier(remote)
                self.assertNotEqual(result.returncode, 0)

    def test_rejects_api_failure_and_non_regular_local_asset(self) -> None:
        result = self.run_verifier([], api_failure=True)
        self.assertNotEqual(result.returncode, 0)
        (self.assets / "directory").mkdir()
        result = self.run_verifier([])
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
