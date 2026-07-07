#!/usr/bin/env python3
"""MiniMax cache-behavior probe — reproduces the empirical evidence
that supports `cache_placement::VolatilePlacement::CurrentUserOnly`.

This script is **not wired into `cargo test`**: it makes live HTTPS
calls to `api.minimaxi.com` and costs a few cents per run. Run it
manually when you want to re-verify the `StrictHistoryMatch` claim
against a current MiniMax deployment (behavior can change as the
vendor updates their cache infrastructure).

## What it proves

Scenario V: a volatile preamble in msg[1] advances each round (the
shape astra used to emit — `## Self-Awareness\nTurn: N | Tokens: ...`).
Scenario F: the preamble is frozen (the shape astra emits today, with
`CurrentUserOnly` suppressing the volatile block).

Scenarios V and F both append (assistant_tc, tool_result) pairs at
the tail each round. Only msg[1]'s content differs between them.

Expected (StrictHistoryMatch hypothesis):
  V: r0=hit, r1..=0 (history cache invalidates on msg[1] change)
  F: r0=hit, r1..=hit (history cache survives across tool-loop rounds)

Captured on 2026-05-08 (MiniMax-M2.7, `api.minimaxi.com/v1`):
  V: 576 / 0 / 0 / 0
  F: 443 / 443 / 0* / 443   (* sporadic eviction noise)

## How to run

    export MINIMAX_API_KEY=sk-...
    python3 minimax_cache_probe.py

Prints a per-round table and a verdict line. Total cost ~10-15 calls
at MiniMax-M2.7 streaming rates.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

API_KEY = os.environ.get("MINIMAX_API_KEY")
if not API_KEY:
    print("ERROR: set MINIMAX_API_KEY", file=sys.stderr)
    sys.exit(1)

BASE = "https://api.minimaxi.com/v1"
MODEL = "MiniMax-M2.7"

SYSTEM_PROMPT = (
    "You are a helpful assistant. Always answer concisely." * 40
)  # ~400 tokens, large enough that cache_read > 0 is unambiguous.


def call(messages, tools, tag):
    body = {
        "model": MODEL,
        "messages": messages,
        "max_tokens": 50,
        "temperature": 0,
    }
    if tools:
        body["tools"] = tools
    req = urllib.request.Request(
        f"{BASE}/chat/completions",
        data=json.dumps(body).encode(),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {API_KEY}",
        },
    )
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            data = json.loads(r.read())
    except urllib.error.HTTPError as e:
        print(f"  [{tag}] HTTP ERROR: {e.read().decode()[:300]}")
        raise
    dt = (time.time() - t0) * 1000
    u = data.get("usage", {})
    inp = u.get("prompt_tokens") or 0
    cached = (
        u.get("prompt_cache_hit_tokens")
        or u.get("cached_tokens")
        or (u.get("prompt_tokens_details") or {}).get("cached_tokens")
        or 0
    )
    print(f"  [{tag:50s}] prompt={inp} cached={cached} dt={dt:.0f}ms")
    return {"input": inp, "cached": cached, "raw": u}


TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "get_time",
            "description": "Get current time",
            "parameters": {"type": "object", "properties": {}},
        },
    },
]


def build(round_idx: int, preamble_advances: bool):
    """Build messages at round N of a tool loop.

    If `preamble_advances`, msg[1] includes a counter that changes each
    round (the astra-before-fix emission shape). If not, msg[1] is byte-
    identical across rounds (the astra-after-`CurrentUserOnly` shape).
    """
    if preamble_advances:
        preamble = (
            f"<reminder>Turn: {round_idx} | Tokens: "
            f"{100 + round_idx * 10}/8000</reminder>"
        )
    else:
        preamble = ""
    msgs = [
        {"role": "system", "content": SYSTEM_PROMPT},
        {"role": "user", "content": preamble + "\nWhat time is it? Use the tool."},
    ]
    for i in range(round_idx):
        msgs.append(
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": f"call_{i}",
                        "type": "function",
                        "function": {"name": "get_time", "arguments": "{}"},
                    }
                ],
            }
        )
        msgs.append(
            {
                "role": "tool",
                "tool_call_id": f"call_{i}",
                "content": f"2026-05-08T12:00:{i:02d}Z",
            }
        )
    return msgs


def main():
    print("=" * 60)
    print("Scenario V: preamble advances each round (astra-before-fix)")
    print("Expectation if StrictHistoryMatch: round 1+ cached = 0")
    print("=" * 60)
    call(build(0, True), TOOLS, "V warm prime round 0")
    time.sleep(3)
    r0 = call(build(0, True), TOOLS, "V round 0 (should hit full)")
    time.sleep(3)
    r1 = call(build(1, True), TOOLS, "V round 1 (preamble Turn:1)")
    time.sleep(3)
    r2 = call(build(2, True), TOOLS, "V round 2 (preamble Turn:2)")
    time.sleep(3)
    r3 = call(build(3, True), TOOLS, "V round 3 (preamble Turn:3)")

    print()
    print("=" * 60)
    print("Scenario F: preamble frozen (like astra-after-CurrentUserOnly)")
    print("=" * 60)
    call(build(0, False), TOOLS, "F warm prime round 0")
    time.sleep(3)
    f0 = call(build(0, False), TOOLS, "F round 0")
    time.sleep(3)
    f1 = call(build(1, False), TOOLS, "F round 1")
    time.sleep(3)
    f2 = call(build(2, False), TOOLS, "F round 2")
    time.sleep(3)
    f3 = call(build(3, False), TOOLS, "F round 3")

    print()
    print("=" * 60)
    print("Verdict")
    print("=" * 60)
    print(
        f"  V (advancing preamble):  r0={r0['cached']}  r1={r1['cached']}  "
        f"r2={r2['cached']}  r3={r3['cached']}"
    )
    print(
        f"  F (frozen preamble):     r0={f0['cached']}  r1={f1['cached']}  "
        f"r2={f2['cached']}  r3={f3['cached']}"
    )
    print()

    v_tool_rounds_avg = sum(r["cached"] for r in [r1, r2, r3]) / 3
    f_tool_rounds_avg = sum(r["cached"] for r in [f1, f2, f3]) / 3
    print(
        f"  Avg cached on tool-loop rounds:  V={v_tool_rounds_avg:.0f}  "
        f"F={f_tool_rounds_avg:.0f}"
    )

    if v_tool_rounds_avg < 50:
        print("  → V collapses to ~0: StrictHistoryMatch confirmed.")
        print("     CurrentUserOnly is the correct mitigation.")
    elif v_tool_rounds_avg < f_tool_rounds_avg * 0.6:
        print(
            "  → V significantly less than F: advancing preamble DOES hurt cache."
        )
        print("     CurrentUserOnly-style suppression recovers real value.")
    else:
        print("  → V ≈ F: advancing preamble does NOT hurt cache.")
        print(
            "     If this repeats on multiple runs, CurrentUserOnly may be "
            "obsolete."
        )


if __name__ == "__main__":
    main()
