"""Offline checks for memory-article metrics; never loads provider credentials."""
import unittest
import json
from pathlib import Path

from analyze_memory_article import latency, summarize, usage_cost

FLASH_PRICE = {"input": 1e-6, "output": 2e-6, "cache_read": .05e-6}


class MemoryArticleMetricsTest(unittest.TestCase):
    def test_nearest_rank_and_empty_latency(self):
        self.assertEqual(latency(list(range(1, 21))), {"n": 20, "p50_ms": 10.5, "p95_ms": 19})
        self.assertEqual(latency([]), {"n": 0, "p50_ms": None, "p95_ms": None})

    def test_unknown_usage_is_not_zero_cost_coverage(self):
        result = usage_cost([{"status": "unavailable"}], FLASH_PRICE)
        self.assertFalse(result["estimate_complete"])
        self.assertEqual(result["input_output_coverage"], 0)
        self.assertEqual(result["tokens"], {})

    def test_disjoint_input_lanes_and_unreported_cache(self):
        call = {"usage": {"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10},
                "usage_presence": {"fresh": True, "cache_read": True, "output": True}}
        result = usage_cost([call], FLASH_PRICE)
        self.assertEqual(result["tokens"]["reported_input"], 140)
        self.assertAlmostEqual(result["list_price_estimate_usd"], .000122)
        call["usage_presence"]["cache_read"] = False
        self.assertEqual(usage_cost([call], FLASH_PRICE)["cache_read_coverage"], 0)
        self.assertFalse(usage_cost([call], FLASH_PRICE)["estimate_complete"])

    def test_selection_and_answer_are_independent_metrics(self):
        rows = [{"case": "test", "arm": "no_jev", "expected": [0], "selected": [0, 1],
                 "selection_exact": False, "selector_calls": [], "selection_ms": 0,
                 "selection": {"reason": "no_selector", "method": "lexical", "candidates": [{}, {}]},
                 "grade": {"pass": True}, "answer": {"elapsed_ms": 20},
                 "total_ms": 20, "injected_chars": 60}]
        result = summarize(rows, {"jev": {"input": 0, "output": 0, "cache_read": None}, "flash": FLASH_PRICE})
        self.assertEqual(result["selection_exact"], 0)
        self.assertEqual(result["answer_pass"], 1)
        self.assertEqual((result["tp"], result["fp"], result["fn"]), (1, 1, 0))
        self.assertFalse(result["answer_cost"]["estimate_complete"])

    def test_scale_fixtures_keep_two_labels_and_fit_production_windows(self):
        from memory_article_scale_cases import build_cases
        cases = build_cases()
        self.assertEqual(len(cases), 8)
        self.assertEqual({len(c["candidates"]) for c in cases}, {6, 24, 96, 256})
        for c in cases:
            self.assertEqual(len(c["expected"]), 2)
            self.assertLessEqual(len(c["user_message"]), 200)
            self.assertTrue(all(len(s) <= 150 for s in c["candidates"]))

    def test_dated_official_price_card_is_usd_and_applies_cache_discount(self):
        root = Path(__file__).resolve().parents[1]
        card = json.loads((root / "fixtures/memory-article-prices-20260920.json").read_text())
        self.assertEqual(card["currency"], "USD")
        self.assertEqual(card["flash"], {"input": .15e-6, "cache_read": .003e-6, "output": .6e-6})
        self.assertEqual(card["jev"], {"input": .042e-6, "cache_read": None, "output": 0})
        call = {"usage": {"input_tokens": 100, "cached_input_tokens": 900, "output_tokens": 10},
                "usage_presence": {"fresh": True, "cache_read": True, "output": True}}
        result = usage_cost([call], card["flash"])
        self.assertAlmostEqual(result["list_price_estimate_usd"], .0000237)
        self.assertAlmostEqual(result["all_input_cache_miss_scenario_usd"], .000156)
        self.assertTrue(result["estimate_complete"])

    def test_recall_suite_has_partial_and_negative_controls_without_truncation(self):
        root = Path(__file__).resolve().parents[1]
        cases = json.loads((root / "fixtures/memory-injection-recall.json").read_text())
        self.assertEqual(len(cases), 8)
        self.assertEqual(len({c["id"] for c in cases}), 8)
        self.assertTrue(any(not c["expected"] for c in cases))
        self.assertTrue(any(len(c["expected"]) >= 3 for c in cases))
        for c in cases:
            self.assertLessEqual(len(c["user_message"]), 200)
            self.assertTrue(all(len(s) <= 150 for s in c["candidates"]))
            self.assertEqual(len(set(c["expected"])), len(c["expected"]))
            self.assertTrue(all(0 <= i < len(c["candidates"]) for i in c["expected"]))


if __name__ == "__main__":
    unittest.main()
