from pathlib import Path
import tempfile
import unittest

from select_harness_cases import select


class SelectHarnessCasesTests(unittest.TestCase):
    def test_invalid_list_never_stages_a_partial_suite(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cases = root / "cases"
            cases.mkdir()
            (cases / "valid.yaml").write_text("name: valid\n")
            (root / "outside.yaml").write_text("name: outside\n")
            (cases / "linked.yaml").symlink_to(root / "outside.yaml")
            (cases / "nested").mkdir()
            (cases / "nested/valid.yaml").write_text("name: collision\n")
            manifest = root / "list.txt"
            destination = root / "selected"
            for entries in ("# empty\n", "valid.yaml\nvalid.yaml", "valid.yaml\nmissing.yaml",
                            "../outside.yaml", "linked.yaml", "valid.yaml\nnested/valid.yaml"):
                with self.subTest(entries=entries):
                    manifest.write_text(entries)
                    with self.assertRaises(ValueError):
                        select(cases, manifest, destination)
                    self.assertFalse(destination.exists())
            manifest.write_text("valid.yaml")
            select(cases, manifest, destination)
            self.assertEqual((destination / "valid.yaml").read_bytes(), (cases / "valid.yaml").read_bytes())
            with self.assertRaises(FileExistsError):
                select(cases, manifest, destination)
