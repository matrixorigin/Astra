#!/usr/bin/env python3
"""Analyze the opt-in Rust memory article harness; no network or credentials.

Latency percentiles use nearest rank; repeated fixtures are not independent cases.
Prices come from an explicit dated price card, including reported cache hits,
NOT invoices. Missing usage remains explicitly unknown.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import statistics


def latency(values):
    values = sorted(values)
    return {"n": len(values), "p50_ms": statistics.median(values) if values else None,
            "p95_ms": values[math.ceil(len(values) * .95) - 1] if values else None}


def usage_cost(calls, price):
    totals = Counter()
    complete = 0
    cache_known = 0
    estimate = 0.0
    cold_input_estimate = 0.0
    for call in calls:
        usage = call.get("usage", {})
        presence = call.get("usage_presence", {})
        if presence.get("fresh") and presence.get("output"):
            complete += 1
            # Native JEV usage has no cache evidence; its prompt count is reported
            # input, not proof of a cache hit/miss. Do not infer cache rate.
            input_count = sum(usage.get(k, 0) for k in
                              ("input_tokens", "cached_input_tokens", "cache_creation_tokens"))
            output_count = usage["output_tokens"]
            totals["reported_input"] += input_count
            totals["reported_output"] += output_count
            cold_input_estimate += input_count * price["input"] + output_count * price["output"]
            cache_count = usage.get("cached_input_tokens", 0)
            if price["cache_read"] is not None and not presence.get("cache_read"):
                complete -= 1
            else:
                estimate += (input_count - cache_count) * price["input"] + output_count * price["output"]
                estimate += cache_count * (price["cache_read"] if price["cache_read"] is not None else price["input"])
        if presence.get("cache_read"):
            cache_known += 1
            totals["reported_cache_read"] += usage.get("cached_input_tokens", 0)
    return {"calls": len(calls), "input_output_coverage": complete,
            "cache_read_coverage": cache_known, "tokens": dict(totals),
            "list_price_estimate_usd": estimate,
            "all_input_cache_miss_scenario_usd": cold_input_estimate,
            "estimate_complete": complete == len(calls),
            "cache_discount_applied": price["cache_read"] is not None}


def summarize(rows, prices):
    expected, actual, fp, fn, tp = set(), set(), 0, 0, 0
    for r in rows:
        expected, actual = set(r["expected"]), set(r["selected"])
        tp += len(expected & actual)
        fp += len(actual - expected)
        fn += len(expected - actual)
    calls = [c for r in rows for c in r["selector_calls"]]
    arm = rows[0]["arm"]
    selector_price = prices["jev"] if arm == "jev" else prices["flash"]
    return {
        "rows": len(rows), "unique_cases": len({r["case"] for r in rows}),
        "selection_exact": sum(r["selection_exact"] for r in rows),
        "answer_pass": sum(r["grade"]["pass"] for r in rows),
        "answer_unavailable_or_incomplete": sum(bool(r["grade"].get("unavailable_or_incomplete")) for r in rows),
        "selection_reason": dict(Counter(r["selection"]["reason"] for r in rows)),
        "selector_finish": dict(Counter(str(c.get("finish_reason")) for c in calls)),
        "tp": tp, "fp": fp, "fn": fn,
        "candidate_decisions": sum(len(r["selection"]["candidates"]) for r in rows),
        "selected_total": sum(len(r["selected"]) for r in rows),
        "mean_false_positives": fp / len(rows),
        "mean_injected_chars": sum(r["injected_chars"] for r in rows) / len(rows),
        "precision": tp / (tp + fp) if tp + fp else None,
        "recall": tp / (tp + fn) if tp + fn else None,
        "selector_latency_all": latency([r["selection_ms"] for r in rows]),
        "selector_latency_valid": latency([r["selection_ms"] for r in rows if r["selection"]["method"] == "model"]),
        "answer_latency": latency([r["answer"]["elapsed_ms"] for r in rows]),
        "total_latency": latency([r["total_ms"] for r in rows]),
        "injected_chars_total": sum(r["injected_chars"] for r in rows),
        "selector_cost": usage_cost(calls, selector_price),
        "answer_cost": usage_cost([r["answer"] for r in rows], prices["flash"]),
    }


def analyze(run, price_card):
    complete = json.loads((run / "complete.json").read_text())
    manifest = json.loads((run / "manifest.json").read_text())
    cases_bytes = (run / "cases.json").read_bytes()
    # cases.json is pretty-printed; validate semantic identity against the
    # per-row frozen IDs/labels, not against the input's whitespace-dependent SHA.
    cases = {c["id"]: c for c in json.loads(cases_bytes)}
    rows = [json.loads(s) for s in (run / "results.jsonl").read_text().splitlines()]
    if not complete["completed"] or len(rows) != complete["rows"]:
        raise ValueError("incomplete run")
    seen = set()
    for r in rows:
        key = (r["repeat"], r["case"], r["arm"])
        if key in seen or r["case"] not in cases:
            raise ValueError("duplicate / unknown row")
        seen.add(key)
        if sorted(r["expected"]) != sorted(cases[r["case"]]["expected"]):
            raise ValueError("labels changed")
    expected_keys = {(rep, cid, arm) for rep in range(manifest["repeat"])
                     for cid in cases for arm in ("no_jev", "jev", "flash_jev_like")}
    if seen != expected_keys:
        raise ValueError("incomplete case/repeat/arm matrix")
    if (run / "cases.input.json").exists():
        frozen = (run / "cases.input.json").read_bytes()
        if hashlib.sha256(frozen).hexdigest() != manifest["cases_sha256"]:
            raise ValueError("input fixture hash mismatch")
        if json.loads(frozen) != list(cases.values()):
            raise ValueError("frozen cases differ")
    if price_card["currency"] != "USD" or price_card["unit"] != "USD per token":
        raise ValueError("explicit USD-per-token price card required")
    prices = {k: price_card[k] for k in ("jev", "flash")}
    for price in prices.values():
        for key in ("input", "cache_read", "output"):
            if key == "cache_read" and price[key] is None:
                continue
            if not isinstance(price[key], (int, float)) or not math.isfinite(price[key]) or price[key] < 0:
                raise ValueError("invalid price")
    summary = {"run": run.name, "commit": manifest["commit"],
               "provenance": {k: manifest.get(k) for k in
                              ("started_at", "binary_sha256", "sources", "cases_sha256", "features", "repeat",
                               "scope", "selector_deadline_ms", "answer_deadline_ms")},
               "results_sha256": hashlib.sha256((run / "results.jsonl").read_bytes()).hexdigest(),
               "price_basis": "dated official USD price card; reported cache discounts; unknown usage not zero",
               "price_card": price_card,
               "prices_per_token": prices, "groups": {}, "case_results": {}}
    groups = {
        "all": rows, "core": [r for r in rows if not r["stress"]],
        "stress": [r for r in rows if r["stress"]],
        "core_relevance": [r for r in rows if not r["stress"] and not r["dismissal"]],
        "core_dismissal": [r for r in rows if not r["stress"] and r["dismissal"]],
    }
    sizes = {len(r["selection"]["candidates"]) for r in rows}
    if len(sizes) > 1:
        for size in sorted(sizes):
            groups[f"candidates_{size}"] = [r for r in rows if len(r["selection"]["candidates"]) == size]
    for group, subset in groups.items():
        summary["groups"][group] = {arm: summarize([r for r in subset if r["arm"] == arm], prices)
                                    for arm in sorted({r["arm"] for r in subset})}
    for cid in cases:
        summary["case_results"][cid] = {
            arm: [{"repeat": r["repeat"], "selected": r["selected"],
                   "selection_exact": r["selection_exact"], "reason": r["selection"]["reason"],
                   "answer_pass": r["grade"]["pass"], "answer": r["answer"].get("text")}
                  for r in rows if r["case"] == cid and r["arm"] == arm]
            for arm in sorted({r["arm"] for r in rows})}
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--price-card", type=Path, required=True)
    args = parser.parse_args()
    result = analyze(args.run, json.loads(args.price_card.read_text()))
    if args.output:
        with args.output.open("x") as stream:
            json.dump(result, stream, ensure_ascii=False, indent=2)
            stream.write("\n")
    for group, arms in result["groups"].items():
        print(group)
        for arm, s in arms.items():
            print(arm, json.dumps({k: s[k] for k in ["rows", "selection_exact", "answer_pass",
                "selection_reason", "selector_latency_all", "total_latency", "selector_cost", "answer_cost"]}, ensure_ascii=False))


if __name__ == "__main__":
    main()
