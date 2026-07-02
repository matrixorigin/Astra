#!/usr/bin/env python3
"""Run the live MatrixOne durable run-event pressure probe.

The probe drives an ignored Rust test that writes many concurrent completed runs
with large streaming outputs directly into the durable run-event persistence
path. It avoids real LLM/provider limits so the result measures DB row budget,
compaction, and replay behavior instead of external API quota.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


RESULT_RE = re.compile(r"^DURABLE_EVENT_PRESSURE_RESULT\s+({.*})$", re.MULTILINE)

PROFILE_DEFAULTS = {
    "smoke": {
        "runs": 3,
        "text_deltas": 1_001,
        "progress_rows": 525,
    },
    "pressure": {
        "runs": 100,
        "text_deltas": 10_000,
        "progress_rows": 525,
    },
}


@dataclass(frozen=True)
class ProbeCommand:
    name: str
    command: list[str]
    database: str
    env: dict[str, str]


def parse_pressure_results(text: str) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for match in RESULT_RE.finditer(text):
        results.append(json.loads(match.group(1)))
    return results


def safe_database_name(value: str) -> str:
    if not value:
        raise ValueError("database name cannot be empty")
    if not all(ch.isascii() and (ch.isalnum() or ch == "_") for ch in value):
        raise ValueError(f"database name must be a simple identifier: {value!r}")
    if "test" not in value and "smoke" not in value:
        raise ValueError(
            f"refusing durable event pressure probe against non-test database name: {value}"
        )
    return value


def positive_int(value: int | None, default: int) -> int:
    resolved = default if value is None else value
    if resolved <= 0:
        raise ValueError(f"value must be positive: {resolved}")
    return resolved


def profile_defaults(profile: str) -> dict[str, int]:
    try:
        return PROFILE_DEFAULTS[profile].copy()
    except KeyError as exc:
        raise ValueError(f"unknown profile: {profile}") from exc


def build_command(args: argparse.Namespace) -> ProbeCommand:
    defaults = profile_defaults(args.profile)
    runs = positive_int(args.runs, defaults["runs"])
    text_deltas = positive_int(args.text_deltas, defaults["text_deltas"])
    progress_rows = positive_int(args.progress_rows, defaults["progress_rows"])
    database = safe_database_name(args.database)

    env = {
        "ASTRA_TEST_DB_IT": "1",
        "ASTRA_AUTO_CREATE_DATABASE": "1",
        "ASTRA_DATABASE_PREFIX": os.environ.get("ASTRA_DATABASE_PREFIX", ""),
        "ASTRA_DATABASE": database,
        "ASTRA_DURABLE_EVENT_PRESSURE_RUNS": str(runs),
        "ASTRA_DURABLE_EVENT_PRESSURE_TEXT_DELTAS": str(text_deltas),
        "ASTRA_DURABLE_EVENT_PRESSURE_PROGRESS_ROWS": str(progress_rows),
    }
    if args.row_budget is not None:
        env["ASTRA_DURABLE_RUN_EVENT_BATCH_MAX_ROWS"] = str(
            positive_int(args.row_budget, args.row_budget)
        )
    if args.byte_budget is not None:
        env["ASTRA_DURABLE_RUN_EVENT_BATCH_MAX_BYTES"] = str(
            positive_int(args.byte_budget, args.byte_budget)
        )
    command = [
        "cargo",
        "test",
        "--manifest-path",
        "rust/Cargo.toml",
        "-p",
        "astra-runtime",
        "--lib",
        "durable_run_event_pressure_probe",
        "--",
        "--ignored",
        "--nocapture",
    ]
    return ProbeCommand(
        name="durable_event_pressure",
        command=command,
        database=database,
        env=env,
    )


def run_command(probe: ProbeCommand, output_dir: Path) -> dict[str, Any]:
    env = os.environ.copy()
    env.update(probe.env)
    started = time.monotonic()
    completed = subprocess.run(
        probe.command,
        cwd=Path(__file__).resolve().parents[2],
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    elapsed_ms = int((time.monotonic() - started) * 1000)
    (output_dir / f"{probe.name}.stdout.log").write_text(completed.stdout, encoding="utf-8")
    (output_dir / f"{probe.name}.stderr.log").write_text(completed.stderr, encoding="utf-8")
    combined = f"{completed.stdout}\n{completed.stderr}"
    results = parse_pressure_results(combined)
    return {
        "name": probe.name,
        "database": probe.database,
        "command": probe.command,
        "returncode": completed.returncode,
        "elapsed_ms": elapsed_ms,
        "results": results,
    }


def build_summary(args: argparse.Namespace, command_result: dict[str, Any]) -> dict[str, Any]:
    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "profile": args.profile,
        "database": args.database,
        "ok": command_result["returncode"] == 0 and len(command_result["results"]) == 1,
        "command": command_result,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILE_DEFAULTS), default="smoke")
    parser.add_argument(
        "--database",
        default="astra_runtime_test_durable_event_pressure",
        help="MatrixOne database name; must contain 'test' or 'smoke'",
    )
    parser.add_argument("--runs", type=int, help="concurrent durable runs to write")
    parser.add_argument("--text-deltas", type=int, help="live-only text_delta chunks per run")
    parser.add_argument("--progress-rows", type=int, help="durable semantic progress rows per run")
    parser.add_argument("--row-budget", type=int, help="override durable run event row budget")
    parser.add_argument("--byte-budget", type=int, help="override durable run event byte budget")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="directory for logs and summary; defaults under tmp/durable-event-pressure/",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = args.output_dir or Path("tmp") / "durable-event-pressure" / timestamp
    output_dir.mkdir(parents=True, exist_ok=True)

    probe = build_command(args)
    print(
        f"running durable event pressure probe against {probe.database}",
        flush=True,
    )
    command_result = run_command(probe, output_dir)
    summary = build_summary(args, command_result)
    summary_path = output_dir / "summary.json"
    summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {summary_path}")
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
