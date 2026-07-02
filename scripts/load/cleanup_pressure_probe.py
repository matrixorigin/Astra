#!/usr/bin/env python3
"""Run live MatrixOne cleanup pressure probes and summarize their results.

The probe orchestrates ignored Rust integration tests. The Rust tests own the
actual fixture setup and cleanup semantics; this runner gives operators one
repeatable command that captures timings and machine-readable output.
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


RESULT_RE = re.compile(r"^CLEANUP_PRESSURE_RESULT\s+({.*})$", re.MULTILINE)

PROFILE_DEFAULTS = {
    "smoke": {"queue_rows": 2_005, "csl_rows": 2_005, "prompt_rows": 2_005, "prompt_keep_rows": 64},
    "pressure": {
        "queue_rows": 10_000,
        "csl_rows": 10_000,
        "prompt_rows": 10_000,
        "prompt_keep_rows": 256,
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
            f"refusing cleanup pressure probe against non-test database name: {value}"
        )
    return value


def profile_defaults(profile: str) -> dict[str, int]:
    try:
        return PROFILE_DEFAULTS[profile].copy()
    except KeyError as exc:
        raise ValueError(f"unknown profile: {profile}") from exc


def build_commands(args: argparse.Namespace) -> list[ProbeCommand]:
    defaults = profile_defaults(args.profile)
    queue_rows = args.queue_rows or defaults["queue_rows"]
    csl_rows = args.csl_rows or defaults["csl_rows"]
    prompt_rows = args.prompt_rows or defaults["prompt_rows"]
    prompt_keep_rows = args.prompt_keep_rows or defaults["prompt_keep_rows"]
    base = safe_database_name(args.database_base)

    common_env = {
        "ASTRA_TEST_DB_IT": "1",
        "ASTRA_AUTO_CREATE_DATABASE": "1",
        "ASTRA_DATABASE_PREFIX": os.environ.get("ASTRA_DATABASE_PREFIX", ""),
    }
    manifest = ["cargo", "test", "--manifest-path", "rust/Cargo.toml"]

    return [
        ProbeCommand(
            name="agent_message_queue",
            database=safe_database_name(f"{base}_queue"),
            env={
                **common_env,
                "ASTRA_DATABASE": f"{base}_queue",
                "ASTRA_CLEANUP_PRESSURE_QUEUE_ROWS": str(queue_rows),
            },
            command=[
                *manifest,
                "-p",
                "astra-messaging",
                "db_transport::tests::db_cleanup_expired_pressure_probe",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        ),
        ProbeCommand(
            name="conversation_log",
            database=safe_database_name(f"{base}_csl"),
            env={
                **common_env,
                "ASTRA_DATABASE": f"{base}_csl",
                "ASTRA_CLEANUP_PRESSURE_CSL_ROWS": str(csl_rows),
            },
            command=[
                *manifest,
                "-p",
                "astra-turn-core",
                "--features",
                "db-store",
                "conversation_log::db_store::tests::db_truncate_gc_pressure_probe",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        ),
        ProbeCommand(
            name="prompt_retention",
            database=safe_database_name(f"{base}_prompt"),
            env={
                **common_env,
                "ASTRA_DATABASE": f"{base}_prompt",
                "ASTRA_CLEANUP_PRESSURE_PROMPT_ROWS": str(prompt_rows),
                "ASTRA_CLEANUP_PRESSURE_PROMPT_KEEP_ROWS": str(prompt_keep_rows),
            },
            command=[
                *manifest,
                "-p",
                "astra-services",
                "--test",
                "prompt_retention_db_it",
                "prompt_retention_pressure_probe",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        ),
    ]


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
    return {
        "name": probe.name,
        "database": probe.database,
        "command": probe.command,
        "returncode": completed.returncode,
        "elapsed_ms": elapsed_ms,
        "results": parse_pressure_results(combined),
    }


def build_summary(args: argparse.Namespace, command_results: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "profile": args.profile,
        "database_base": args.database_base,
        "ok": all(result["returncode"] == 0 for result in command_results),
        "commands": command_results,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILE_DEFAULTS), default="smoke")
    parser.add_argument(
        "--database-base",
        default="astra_runtime_test_cleanup_pressure",
        help="base MatrixOne database name; must contain 'test' or 'smoke'",
    )
    parser.add_argument("--queue-rows", type=int, help="expired agent_message_queue rows")
    parser.add_argument("--csl-rows", type=int, help="conversation_log rows before truncate boundary")
    parser.add_argument("--prompt-rows", type=int, help="eligible prompt_request_records rows")
    parser.add_argument("--prompt-keep-rows", type=int, help="active-session prompt rows to guard")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="directory for logs and summary; defaults under tmp/cleanup-pressure/",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = args.output_dir or Path("tmp") / "cleanup-pressure" / timestamp
    output_dir.mkdir(parents=True, exist_ok=True)

    command_results = []
    for probe in build_commands(args):
        print(f"running {probe.name} pressure probe against {probe.database}", flush=True)
        command_results.append(run_command(probe, output_dir))

    summary = build_summary(args, command_results)
    summary_path = output_dir / "summary.json"
    summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {summary_path}")
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
