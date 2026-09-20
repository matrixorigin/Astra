#!/usr/bin/env python3
"""Export structured synthetic-run evidence without credential-bearing config.

All cases and all repetitions are retained. The output includes actual judgment
payloads and responses, answer responses, decisions, timings and reported usage.
Review user-supplied fixtures before publishing; this is not a generic PII scrubber.
"""
import argparse
import hashlib
import json
from pathlib import Path

from analyze_memory_article import analyze


def export(run, price_card):
    summary = analyze(run, price_card)  # validate complete matrix and frozen labels
    rows = [json.loads(line) for line in (run / "results.jsonl").read_text().splitlines()]
    # Explicit allowlist; never copy model configuration, process environment or logs.
    evidence = []
    requests = {}
    for row in rows:
        selectors = []
        for call in row["selector_calls"]:
            request = next(m["content"] for m in call["messages"] if m["role"] == "user")
            # Keep the exact serialized payload for byte-for-byte hash checks.
            # A parsed view is derived with json.loads(raw), not stored twice.
            json.loads(request)  # Reject malformed requests before publication.
            request_id = hashlib.sha256(request.encode()).hexdigest()
            requests[request_id] = {"raw": request}
            selectors.append({"request_sha256": request_id,
                              **{key: call[key] for key in ("status", "text", "model", "error_kind",
                                  "elapsed_ms", "usage", "usage_presence", "finish_reason") if key in call}})
        evidence.append({**{key: row[key] for key in ("case", "arm", "repeat", "expected", "selected",
                            "selection", "grade", "selection_ms", "total_ms")},
                         "selector_calls": selectors,
                         "answer": {key: row["answer"][key] for key in ("status", "text", "model", "error_kind",
                             "elapsed_ms", "usage", "usage_presence", "finish_reason") if key in row["answer"]}})
    return {"scope": "Synthetic component evaluation; no user traffic or credentials",
            "raw_results_sha256": summary["results_sha256"],
            "provenance": summary["provenance"],
            "cases": json.loads((run / "cases.json").read_text()),
            "requests": requests,
            "rows": evidence}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    parser.add_argument("--price-card", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = export(args.run, json.loads(args.price_card.read_text()))
    with args.output.open("x") as stream:
        json.dump(result, stream, ensure_ascii=False, separators=(",", ":"))
        stream.write("\n")
    print(f"Exported {len(result['rows'])} rows; review synthetic inputs before publishing.")


if __name__ == "__main__":
    main()
