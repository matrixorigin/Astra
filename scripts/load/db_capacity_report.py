#!/usr/bin/env python3
"""Summarize capacity probe output into DB pressure and release-gate verdicts."""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


NETWORK_HINTS = (
    "network",
    "connection",
    "connect",
    "provider",
    "refused",
    "reset",
    "socket",
    "closed",
)


@dataclass(frozen=True)
class Thresholds:
    db_pressure_pool_waits: int = 1
    db_saturation_pool_waits: int = 50
    db_saturation_pool_timeouts: int = 1
    db_pressure_slow_sql: int = 50
    db_saturation_slow_sql: int = 100
    broad_slowdown_distinct_sql: int = 4
    broad_slowdown_shapes: int = 2
    slow_elapsed_warning_secs: float = 10.0


def load_json(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as handle:
        data = json.load(handle)
    if not isinstance(data, dict):
        raise ValueError(f"{path} did not contain a JSON object")
    return data


def as_int(value: Any) -> int:
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    return 0


def as_float(value: Any) -> float:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    return 0.0


def dict_int(data: Any) -> dict[str, int]:
    if not isinstance(data, dict):
        return {}
    return {str(key): as_int(value) for key, value in data.items()}


def list_str(data: Any) -> list[str]:
    if not isinstance(data, list):
        return []
    return [str(item) for item in data]


def sql_shape(summary: str) -> str:
    first = summary.strip().split(" ", 1)[0].upper()
    if first in {"SELECT", "INSERT", "UPDATE", "DELETE", "COMMIT", "BEGIN", "ROLLBACK"}:
        return first
    return "OTHER"


def summarize_sql_shapes(slow_sql_by_summary: dict[str, int]) -> dict[str, int]:
    shapes: dict[str, int] = {}
    for summary, count in slow_sql_by_summary.items():
        shape = sql_shape(summary)
        shapes[shape] = shapes.get(shape, 0) + count
    return dict(sorted(shapes.items()))


def max_top_elapsed(slow_sql_summary: dict[str, Any]) -> float:
    top_rows = slow_sql_summary.get("top_slow_sql")
    if not isinstance(top_rows, list):
        return 0.0
    elapsed = [
        as_float(row.get("elapsed_secs"))
        for row in top_rows
        if isinstance(row, dict)
    ]
    return max(elapsed, default=0.0)


def has_external_failure_hint(summary: dict[str, Any]) -> bool:
    haystack: list[str] = []
    for key in ("error_codes", "failure_reasons", "outcomes", "terminal_statuses"):
        value = summary.get(key)
        if isinstance(value, dict):
            haystack.extend(str(item).lower() for item in value.keys())
    return any(any(hint in item for hint in NETWORK_HINTS) for item in haystack)


def build_report(
    probe_summary: dict[str, Any],
    slow_sql_summary: dict[str, Any] | None,
    thresholds: Thresholds,
) -> dict[str, Any]:
    total = as_int(probe_summary.get("total"))
    completed = as_int(probe_summary.get("completed"))
    failed = as_int(probe_summary.get("failed"))
    contract_violations = list_str(probe_summary.get("contract_violations"))
    error_codes = dict_int(probe_summary.get("error_codes"))
    failure_reasons = dict_int(probe_summary.get("failure_reasons"))

    slow_sql_summary = slow_sql_summary or {}
    slow_sql_count = as_int(slow_sql_summary.get("slow_sql_count"))
    pool_wait_count = as_int(slow_sql_summary.get("pool_wait_count"))
    pool_wait_by_kind = dict_int(slow_sql_summary.get("pool_wait_by_kind"))
    pool_timeout_count = pool_wait_by_kind.get("pool_timeout", 0)
    slow_sql_by_summary = dict_int(slow_sql_summary.get("slow_sql_by_summary"))
    sql_shapes = summarize_sql_shapes(slow_sql_by_summary)
    distinct_slow_sql_summaries = len(slow_sql_by_summary)
    distinct_sql_shapes = len([count for count in sql_shapes.values() if count > 0])
    top_elapsed_secs = max_top_elapsed(slow_sql_summary)

    broad_slowdown = (
        distinct_slow_sql_summaries >= thresholds.broad_slowdown_distinct_sql
        and distinct_sql_shapes >= thresholds.broad_slowdown_shapes
        and top_elapsed_secs >= thresholds.slow_elapsed_warning_secs
    )
    db_pressure = (
        pool_wait_count >= thresholds.db_pressure_pool_waits
        or slow_sql_count >= thresholds.db_pressure_slow_sql
        or top_elapsed_secs >= thresholds.slow_elapsed_warning_secs
    )
    db_saturated = (
        pool_timeout_count >= thresholds.db_saturation_pool_timeouts
        or pool_wait_count >= thresholds.db_saturation_pool_waits
        or (
            slow_sql_count >= thresholds.db_saturation_slow_sql
            and broad_slowdown
        )
    )

    admission_limited = error_codes.get("run_admission_timeout", 0) > 0
    incomplete_requests = total > 0 and completed < total
    contract_failed = bool(contract_violations)
    external_failure_suspected = has_external_failure_hint(probe_summary) and not db_saturated

    failure_modes: list[str] = []
    if db_saturated:
        failure_modes.append("db_saturation")
    elif db_pressure:
        failure_modes.append("db_pressure")
    if admission_limited:
        failure_modes.append("admission_limited")
    if contract_failed:
        failure_modes.append("contract_violations")
    if incomplete_requests:
        failure_modes.append("incomplete_requests")
    if external_failure_suspected:
        failure_modes.append("external_or_harness_failure_suspected")

    if db_saturated:
        verdict = "db_saturation_boundary"
    elif external_failure_suspected:
        verdict = "external_or_harness_failure"
    elif admission_limited:
        verdict = "admission_limited"
    elif contract_failed or incomplete_requests:
        verdict = "contract_or_request_failure"
    elif db_pressure:
        verdict = "db_pressure_watch"
    else:
        verdict = "release_safe_capacity_sample"

    release_safe = (
        verdict == "release_safe_capacity_sample"
        and completed == total
        and failed == 0
        and not contract_violations
    )

    caveats: list[str] = []
    base_url = str(probe_summary.get("base_url") or "")
    if "127.0.0.1" in base_url or "localhost" in base_url:
        caveats.append("local_probe_not_production_db_proof")
    if str(probe_summary.get("profile") or "").startswith("500"):
        caveats.append("500_cli_requires_multi_pod_or_staging_release_gate")
    if not slow_sql_summary:
        caveats.append("slow_sql_summary_missing")

    next_steps: list[str] = []
    if db_saturated:
        next_steps.append("capture multi-pod or staging MatrixOne evidence before calling production DB insufficient")
        next_steps.append("inspect pool waits plus DB host CPU, memory, IO, and MatrixOne internal queues")
    elif admission_limited:
        next_steps.append("validate run admission policy separately from DB latency")
    elif external_failure_suspected:
        next_steps.append("fix probe/provider/socket limits before interpreting DB results")
    elif db_pressure:
        next_steps.append("watch DB latency dashboards and repeat at the next concurrency tier")
    else:
        next_steps.append("use this sample as a clean baseline for the next scale tier")

    return {
        "verdict": verdict,
        "release_safe": release_safe,
        "failure_modes": failure_modes,
        "evidence": {
            "total": total,
            "completed": completed,
            "failed": failed,
            "contract_violations": contract_violations,
            "error_codes": error_codes,
            "failure_reasons": failure_reasons,
            "db_pool_wait_count": pool_wait_count,
            "db_pool_wait_by_kind": pool_wait_by_kind,
            "slow_sql_count": slow_sql_count,
            "distinct_slow_sql_summaries": distinct_slow_sql_summaries,
            "sql_shape_counts": sql_shapes,
            "top_elapsed_secs": top_elapsed_secs,
            "broad_slowdown": broad_slowdown,
        },
        "caveats": caveats,
        "next_steps": next_steps,
    }


def format_markdown(report: dict[str, Any]) -> str:
    evidence = report["evidence"]
    lines = [
        f"# DB Capacity Verdict: {report['verdict']}",
        "",
        f"- release_safe: {str(report['release_safe']).lower()}",
        f"- failure_modes: {', '.join(report['failure_modes']) or 'none'}",
        f"- requests: {evidence['completed']}/{evidence['total']} completed, {evidence['failed']} failed",
        f"- db_pool_waits: {evidence['db_pool_wait_count']} {evidence['db_pool_wait_by_kind']}",
        f"- slow_sql: {evidence['slow_sql_count']} across {evidence['distinct_slow_sql_summaries']} summaries",
        f"- sql_shapes: {evidence['sql_shape_counts']}",
        f"- top_elapsed_secs: {evidence['top_elapsed_secs']:.3f}",
        f"- broad_slowdown: {str(evidence['broad_slowdown']).lower()}",
    ]
    if evidence["contract_violations"]:
        lines.append(f"- contract_violations: {', '.join(evidence['contract_violations'])}")
    if evidence["error_codes"]:
        lines.append(f"- error_codes: {evidence['error_codes']}")
    if report["caveats"]:
        lines.append(f"- caveats: {', '.join(report['caveats'])}")
    if report["next_steps"]:
        lines.append(f"- next_steps: {'; '.join(report['next_steps'])}")
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe-summary", type=Path, required=True)
    parser.add_argument("--slow-sql-summary", type=Path)
    parser.add_argument("--format", choices=("json", "markdown"), default="json")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--require-release-safe", action="store_true")
    parser.add_argument("--db-saturation-pool-waits", type=int, default=50)
    parser.add_argument("--db-saturation-pool-timeouts", type=int, default=1)
    parser.add_argument("--db-saturation-slow-sql", type=int, default=100)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    thresholds = Thresholds(
        db_saturation_pool_waits=args.db_saturation_pool_waits,
        db_saturation_pool_timeouts=args.db_saturation_pool_timeouts,
        db_saturation_slow_sql=args.db_saturation_slow_sql,
    )
    probe_summary = load_json(args.probe_summary)
    slow_summary = load_json(args.slow_sql_summary) if args.slow_sql_summary else None
    report = build_report(probe_summary, slow_summary, thresholds)
    rendered = (
        json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.format == "json"
        else format_markdown(report)
    )
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        sys.stdout.write(rendered)
    if args.require_release_safe and not report["release_safe"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
