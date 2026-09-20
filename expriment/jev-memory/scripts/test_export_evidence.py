import json
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from export_evidence import export


class EvidenceExportTest(unittest.TestCase):
    def test_deduplicates_requests_and_excludes_non_allowlisted_fields(self):
        # Synthetic fixture files are test outputs, not repository edits.
        request = '{"state":{"user_message":"task"},"questions":{}}'
        row = {key: [] for key in ("expected", "selected")}
        row.update(case="fixture", arm="jev", repeat=0, selection={}, grade={}, selection_ms=1, total_ms=2)
        row["selector_calls"] = [{"messages": [{"role": "user", "content": request}],
                                  "status": "response", "text": "{}", "api_key": "SENTINEL"}]
        row["answer"] = {"text": "{}", "api_key": "SENTINEL"}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "results.jsonl").write_text(json.dumps(row) + "\n" + json.dumps(row) + "\n")
            (root / "cases.json").write_text("[]")
            with patch("export_evidence.analyze", return_value={"results_sha256": "raw-hash", "provenance": {}}):
                result = export(root, {})
        self.assertEqual(len(result["rows"]), 2)
        self.assertEqual(len(result["requests"]), 1)
        self.assertNotIn("SENTINEL", json.dumps(result))
        for item in result["rows"]:
            ref = item["selector_calls"][0]["request_sha256"]
            self.assertEqual(result["requests"][ref]["parsed"], json.loads(request))
            self.assertEqual(hashlib.sha256(result["requests"][ref]["raw"].encode()).hexdigest(), ref)


if __name__ == "__main__":
    unittest.main()
