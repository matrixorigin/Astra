#!/usr/bin/env python3
"""Offline by default; explicitly opt in to paid, real-provider reproduction."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

EXPERIMENT = Path(__file__).resolve().parents[1]
REPO = EXPERIMENT.parents[1]
TEST = "memory_hooks::article_eval::memory_injection_article_live"
CARGO = ["cargo", "test", "-p", "astra-runtime", "--lib", "--no-default-features",
         "--features", "live-provider-tests", "--offline"]


def command(args, **kwargs):
    return subprocess.run([str(x) for x in args], cwd=REPO, check=True, **kwargs)


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true", help="authorize paid provider calls")
    parser.add_argument("--models", type=Path, help="private YAML; never printed or copied")
    parser.add_argument("--suite", choices=["all", "main", "regression", "scale", "recall"], default="all")
    parser.add_argument("--repeat", type=int, choices=range(1, 6), default=3)
    parser.add_argument("--price-card", type=Path,
                        default=EXPERIMENT / "fixtures/memory-article-prices-20260920.json")
    args = parser.parse_args(argv)
    if args.live and (args.models is None or not args.models.is_file()):
        parser.error("--live requires an existing --models file")
    if args.models:
        args.models = args.models.resolve()
    args.price_card = args.price_card.resolve()
    return args


def main(argv=None):
    args = parse_args(argv)
    # Validate the price file before any paid work; never inspect credentials here.
    card = json.loads(args.price_card.read_text())
    if card.get("currency") != "USD":
        raise ValueError("analyzer requires a USD price card")
    for model in ("jev", "flash"):
        for lane in ("input", "output", "cache_read"):
            value = card[model][lane]
            if value is not None and (not isinstance(value, (int, float)) or value < 0):
                raise ValueError("price lanes must be nonnegative per-token values or null")
    command([sys.executable, "-m", "unittest", "discover", "-s", EXPERIMENT / "scripts", "-p", "test_*.py"])
    command(["cargo", "test", "-p", "astra-turn-types", "judgment::tests", "--offline"])
    command(CARGO + ["memory_hooks", "--", "--test-threads=1"])
    if not args.live:
        print("Offline checks passed. No credentials loaded or paid model calls made.")
        return
    build = command(CARGO + ["--no-run", "--message-format=json"], capture_output=True, text=True)
    binaries = []
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact" and item.get("executable")
                and item.get("profile", {}).get("test")):
            binaries.append(item["executable"])
    if len(binaries) != 1:
        raise RuntimeError("expected exactly one runtime test executable")
    suites = ["main", "regression", "scale", "recall"] if args.suite == "all" else [args.suite]
    parent = REPO / "target/expriment"
    parent.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="jev-memory-", dir=parent))
    print(f"Paid run output: {output}", flush=True)
    print(f"Pricing snapshot: {card.get('checked_on', 'unspecified')}; not a current-price guarantee", flush=True)
    scale = output / "scale-cases.json"
    command([sys.executable, EXPERIMENT / "scripts/memory_article_scale_cases.py", "--output", scale])
    fixtures = {"main": EXPERIMENT / "fixtures/memory-injection-article-cases.json",
                "regression": EXPERIMENT / "fixtures/memory-injection-article-regression.json",
                "recall": EXPERIMENT / "fixtures/memory-injection-recall.json",
                "scale": scale}
    for suite in suites:
        destination = output / suite
        env = os.environ.copy()
        env.update(ASTRA_MEMORY_EVAL_CASES=str(fixtures[suite]),
                   ASTRA_MEMORY_EVAL_MODELS=str(args.models),
                   ASTRA_MEMORY_EVAL_OUTPUT=str(destination),
                   ASTRA_MEMORY_EVAL_REPEAT=str(args.repeat))
        command([binaries[0], TEST, "--ignored", "--exact", "--nocapture", "--test-threads=1"], env=env)
        command([sys.executable, EXPERIMENT / "scripts/analyze_memory_article.py", destination,
                 "--price-card", args.price_card, "--output", destination / "summary-official.json"])
        command([sys.executable, EXPERIMENT / "scripts/export_evidence.py", destination,
                 "--price-card", args.price_card, "--output", destination / "evidence.json"])
    print(f"Collection and analysis complete: {output}. Completion does not mean model scores passed.")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        # Do not echo environment/credential paths or captured provider output.
        print(f"Step failed (exit {error.returncode}); existing observations retained.", file=sys.stderr)
        sys.exit(error.returncode)
