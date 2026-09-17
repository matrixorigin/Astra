from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/prepare-release-version.py"
SPEC = importlib.util.spec_from_file_location("prepare_release_version", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
PREPARE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREPARE)


class PrepareReleaseVersionTests(unittest.TestCase):
    def test_same_version_preserves_file_formatting(self) -> None:
        manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        match = re.search(
            r'^\[workspace\.package\]\n.*?^version\s*=\s*"([^"]+)"',
            manifest,
            flags=re.MULTILINE | re.DOTALL,
        )
        self.assertIsNotNone(match)
        version = match.group(1)
        for path, rendered in PREPARE.render_updates(version).items():
            self.assertEqual(
                (ROOT / path).read_text(encoding="utf-8"),
                rendered,
                f"same-version rendering changed {path}",
            )

    def test_next_version_reaches_every_release_surface(self) -> None:
        version = "9.8.7-rc.1"
        updates = PREPARE.render_updates(version)
        expected_files = set(PREPARE.VERSION_FILES)
        self.assertEqual(expected_files, set(updates))
        self.assertIn(f'version = "{version}"', updates[Path("Cargo.toml")])
        self.assertIn(
            f"ASTRA_IMAGE=matrixorigin/astra:{version}",
            updates[Path("deployment/all-in-one/.env.example")],
        )
        self.assertGreaterEqual(
            updates[Path("packages/sdk/package-lock.json")].count(
                f'"version": "{version}"'
            ),
            2,
        )
        self.assertGreaterEqual(
            updates[Path("web/package-lock.json")].count(f'"version": "{version}"'),
            3,
        )
        self.assertIn(
            f"ASTRA_IMAGE=matrixorigin/astra:{version}",
            updates[Path(".env.production.example")],
        )

    def test_semver_validation_rejects_ambiguous_versions(self) -> None:
        self.assertEqual("0.2.0-rc.1", PREPARE.normalize_version("v0.2.0-rc.1"))
        for value in ("01.0.0", "0.1.0-01", "0.1.0-rc..1", "0.1"):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                PREPARE.normalize_version(value)


if __name__ == "__main__":
    unittest.main()
