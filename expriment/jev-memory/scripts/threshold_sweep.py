#!/usr/bin/env python3
"""Retrospective selection-only threshold analysis, NOT new model/answer trials."""
import argparse
import json
from pathlib import Path


def sweep(evidence, thresholds):
    cases = {c["id"]: c for c in evidence["cases"]}
    rows = [r for r in evidence["rows"] if r["arm"] == "jev"
            and not cases[r["case"]].get("dismissal", False)
            and not cases[r["case"]].get("stress", False)]
    valid = [r for r in rows if r["selection"]["method"] == "model"
             and all(c.get("probability_bps") is not None for c in r["selection"]["candidates"])]
    result = []
    for threshold in thresholds:
        tp = fp = fn = exact = 0
        for row in valid:
            selected = {c["index"] for c in row["selection"]["candidates"]
                        if c["probability_bps"] > threshold * 10000}
            expected = set(row["expected"])
            exact += selected == expected
            tp += len(selected & expected)
            fp += len(selected - expected)
            fn += len(expected - selected)
        result.append(dict(threshold=threshold, rows=len(valid), excluded=len(rows)-len(valid),
                           selection_exact=exact, tp=tp, fp=fp, fn=fn))
    return {"scope": "Post-hoc native relevance decisions only; no downstream answer or billing claims",
            "results": result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence", type=Path)
    parser.add_argument("--thresholds", type=float, nargs="+", default=[0.3, 0.4, 0.45, 0.5])
    args = parser.parse_args()
    if not all(0 <= t <= 1 for t in args.thresholds):
        parser.error("thresholds must be in [0,1]")
    print(json.dumps(sweep(json.loads(args.evidence.read_text()), args.thresholds), indent=2))


if __name__ == "__main__":
    main()
