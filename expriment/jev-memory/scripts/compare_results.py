#!/usr/bin/env python3
"""Compare two complete, same-fixture summaries without selecting repetitions."""
import argparse
import json
from pathlib import Path


def compare(before, after, group="core"):
    for key in ("cases_sha256", "repeat", "selector_deadline_ms", "answer_deadline_ms"):
        if before["provenance"].get(key) != after["provenance"].get(key):
            raise ValueError(f"not the same evaluation protocol: {key}")
    if before["prices_per_token"] != after["prices_per_token"]:
        raise ValueError("price cards must use the same rates for this comparison")
    result = {}
    for arm in sorted(before["groups"][group]):
        result[arm] = {}
        for name, summary in (("before", before), ("after", after)):
            s = summary["groups"][group][arm]
            cost = s["selector_cost"]
            result[arm][name] = {key: s[key] for key in ("rows", "selection_exact", "answer_pass", "precision",
                "recall", "fp", "fn", "selection_reason", "selector_latency_all", "total_latency")}
            result[arm][name].update(
                mean_judge_input_tokens=cost["tokens"].get("reported_input", 0) / max(cost["calls"], 1),
                judge_usd_per_1000=cost["list_price_estimate_usd"] * 1000 / s["rows"],
                total_usd_per_1000=(cost["list_price_estimate_usd"] + s["answer_cost"]["list_price_estimate_usd"]) * 1000 / s["rows"],
                cost_complete=cost["estimate_complete"] and s["answer_cost"]["estimate_complete"])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--group", default="core")
    args = parser.parse_args()
    print(json.dumps(compare(json.loads(args.before.read_text()), json.loads(args.after.read_text()), args.group),
                     ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
