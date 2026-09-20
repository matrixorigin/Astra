import unittest
from compare_results import compare


class ComparisonTest(unittest.TestCase):
    def test_rejects_different_fixtures_before_comparing_scores(self):
        with self.assertRaisesRegex(ValueError, "cases_sha256"):
            compare({"provenance": {"cases_sha256": "one"}}, {"provenance": {"cases_sha256": "two"}})

    def test_rejects_different_prices(self):
        with self.assertRaisesRegex(ValueError, "same rates"):
            compare({"provenance": {}, "prices_per_token": {"input": 1}},
                    {"provenance": {}, "prices_per_token": {"input": 2}})


if __name__ == "__main__":
    unittest.main()
