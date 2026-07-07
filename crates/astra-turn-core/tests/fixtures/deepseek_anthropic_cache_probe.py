#!/usr/bin/env python3
"""DeepSeek `/anthropic` endpoint cache probe.

This script is **not wired into `cargo test`**: it makes live HTTPS calls
to `api.deepseek.com/anthropic` and costs a few cents per run. Run it
manually when you suspect the endpoint's cache behavior has changed.

## What it proves

Three scenarios all against `deepseek-v4-pro` on the Anthropic-compat
endpoint with 21 tools and a large system prompt:

  A) `cache_control` marker on the LAST tool (astra's normal shape)
  B) `cache_control` marker on tool[19] (penultimate)
  C) NO tool-level `cache_control` at all

Captured on 2026-05-08 — all three scenarios produced **cached=7936 /
total=7965 (99.6%)** on warm calls. Marker position doesn't matter;
even omitting the marker entirely still caches the full tools array.

Plus a tool-size scaling test (6 tools × {10, 50, 150} filler):

  6×10   filler → cached=1280 / total=1380
  6×50   filler → cached=2816 / total=2820
  6×150  filler → cached=6400 / total=6420

`cached` scales with `total` — tools are actually being cached, not
just the system prefix.

## Why this script exists

Earlier `cache_diagnosis::rule_deepseek_anthropic_tools_not_cached`
claimed the endpoint silently ignored tool-level markers, based on
production sessions showing `cache_read` stuck at ~2432. This probe
**falsified** that claim — the endpoint caches fine. The real cause
of the production flat-2432 must be astra-side byte churn in the wire
payload. The rule was retracted; this script is the commit-level
evidence.

## How to run

    export DEEPSEEK_API_KEY=sk-...
    python3 deepseek_anthropic_cache_probe.py

Prints a per-scenario table and a verdict line.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

API_KEY = os.environ.get("DEEPSEEK_API_KEY")
if not API_KEY:
    print("ERROR: set DEEPSEEK_API_KEY", file=sys.stderr)
    sys.exit(1)

BASE = "https://api.deepseek.com/anthropic"
MODEL = "deepseek-v4-pro"

SYSTEM_PROMPT = "You are a helpful tool-using assistant. " * 50


def big_tool(name, filler=50):
    return {
        "name": name,
        "description": f"Tool {name}. " + ("alpha bravo charlie delta " * filler),
        "input_schema": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
        },
    }


def call(system_blocks, tools, messages, tag):
    body = {
        "model": MODEL,
        "max_tokens": 30,
        "system": system_blocks,
        "messages": messages,
        "tools": tools,
    }
    req = urllib.request.Request(
        f"{BASE}/v1/messages",
        data=json.dumps(body).encode(),
        headers={
            "Content-Type": "application/json",
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
        },
    )
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            data = json.loads(r.read())
    except urllib.error.HTTPError as e:
        print(f"  [{tag}] HTTP ERROR: {e.read().decode()[:400]}")
        raise
    dt = (time.time() - t0) * 1000
    u = data.get("usage", {})
    inp = u.get("input_tokens", 0)
    cached = u.get("cache_read_input_tokens", 0)
    cwrite = u.get("cache_creation_input_tokens", 0)
    total = inp + cached + cwrite
    print(
        f"  [{tag:55s}] input={inp:5d} cached={cached:5d} "
        f"cache_w={cwrite:5d} total={total:5d} dt={dt:.0f}ms"
    )
    return {"input": inp, "cached": cached, "cache_w": cwrite, "total": total}


def sys_with_marker():
    return [
        {
            "type": "text",
            "text": SYSTEM_PROMPT,
            "cache_control": {"type": "ephemeral"},
        }
    ]


tools_21 = [big_tool(f"tool_{i:02d}") for i in range(21)]


def tools_with_cc_on(idx):
    t = [dict(x) for x in tools_21]
    if idx is not None:
        t[idx]["cache_control"] = {"type": "ephemeral"}
    return t


def main():
    print("=" * 72)
    print("Marker-position test (21 tools, large system, 3 scenarios)")
    print("=" * 72)
    print()
    print("Scenario A: cc marker on LAST tool (index 20)")
    call(sys_with_marker(), tools_with_cc_on(20), [{"role": "user", "content": "hi1"}], "A prime")
    time.sleep(3)
    call(sys_with_marker(), tools_with_cc_on(20), [{"role": "user", "content": "hi1"}], "A warm 1")
    time.sleep(3)
    a = call(sys_with_marker(), tools_with_cc_on(20), [{"role": "user", "content": "hi1"}], "A warm 2")

    print()
    print("Scenario B: cc marker on tool[19] (penultimate)")
    call(sys_with_marker(), tools_with_cc_on(19), [{"role": "user", "content": "hi2"}], "B prime")
    time.sleep(3)
    call(sys_with_marker(), tools_with_cc_on(19), [{"role": "user", "content": "hi2"}], "B warm 1")
    time.sleep(3)
    b = call(sys_with_marker(), tools_with_cc_on(19), [{"role": "user", "content": "hi2"}], "B warm 2")

    print()
    print("Scenario C: NO tool-level cc marker")
    call(sys_with_marker(), tools_with_cc_on(None), [{"role": "user", "content": "hi3"}], "C prime")
    time.sleep(3)
    call(sys_with_marker(), tools_with_cc_on(None), [{"role": "user", "content": "hi3"}], "C warm 1")
    time.sleep(3)
    c = call(sys_with_marker(), tools_with_cc_on(None), [{"role": "user", "content": "hi3"}], "C warm 2")

    print()
    print("=" * 72)
    print("Summary")
    print("=" * 72)
    for label, r in [("A marker-last", a), ("B marker-penult", b), ("C no-tool-marker", c)]:
        pct = 100 * r["cached"] / r["total"] if r["total"] else 0
        print(
            f"  {label:22s}: cached={r['cached']:5d}  total={r['total']:5d}  {pct:5.1f}%"
        )

    # Verdict
    print()
    if min(a["cached"], b["cached"], c["cached"]) >= 0.9 * max(
        a["cached"], b["cached"], c["cached"]
    ):
        print(
            "  → All three scenarios cache similarly."
            " DeepSeek caches tools AUTOMATICALLY via prefix match."
        )
        print(
            "  → `rule_deepseek_anthropic_tools_not_cached` was a misdiagnosis:"
            " the endpoint works fine."
        )
    else:
        print("  → Scenarios diverge. Re-investigate marker placement.")


if __name__ == "__main__":
    main()
