#!/usr/bin/env python3
"""Extract SQLx slow statement and pool-wait records from astra JSON logs."""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SLOW_SQL_MESSAGE = "slow statement: execution time exceeded alert threshold"
POOL_WAIT_PATTERNS = (
    "pool timed out while waiting for an open connection",
    "acquired connection, but time to acquire exceeded slow threshold",
)


@dataclass(frozen=True)
class SlowSqlRow:
    timestamp: str
    elapsed_secs: float
    summary: str
    rows_affected: int | None
    rows_returned: int | None
    request_id: str
    route: str
    statement: str


@dataclass(frozen=True)
class PoolWaitRow:
    timestamp: str
    level: str
    target: str
    kind: str
    acquire_after_secs: float | None
    request_id: str
    route: str
    message: str


@dataclass
class SlowSqlReport:
    slow_sql_rows: list[SlowSqlRow]
    pool_wait_rows: list[PoolWaitRow]
    json_records: int
    skipped_lines: int
    start: str | None
    end: str | None

    def summary(self, top: int) -> dict[str, Any]:
        slow_counts = Counter(row.summary for row in self.slow_sql_rows)
        pool_counts = Counter(row.kind for row in self.pool_wait_rows)
        top_slow = sorted(self.slow_sql_rows, key=lambda row: row.elapsed_secs, reverse=True)[:top]
        return {
            "start": self.start,
            "end": self.end,
            "json_records": self.json_records,
            "skipped_lines": self.skipped_lines,
            "slow_sql_count": len(self.slow_sql_rows),
            "pool_wait_count": len(self.pool_wait_rows),
            "slow_sql_by_summary": dict(slow_counts.most_common(top)),
            "pool_wait_by_kind": dict(pool_counts.most_common()),
            "top_slow_sql": [
                {
                    "timestamp": row.timestamp,
                    "elapsed_secs": row.elapsed_secs,
                    "summary": row.summary,
                    "request_id": row.request_id or None,
                    "route": row.route or None,
                    "statement": row.statement,
                }
                for row in top_slow
            ],
        }


def parse_timestamp(value: str | None) -> datetime | None:
    if not value:
        return None
    normalized = value.replace("Z", "+00:00")
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def normalize_sql(statement: str) -> str:
    return re.sub(r"[\n\t ]+", " ", statement).strip()


def span_value(record: dict[str, Any], key: str) -> str:
    span = record.get("span")
    if isinstance(span, dict) and isinstance(span.get(key), str):
        return span[key]
    spans = record.get("spans")
    if isinstance(spans, list):
        for item in reversed(spans):
            if isinstance(item, dict) and isinstance(item.get(key), str):
                return item[key]
    return ""


def pool_wait_kind(message: str) -> str:
    if "acquired connection, but time to acquire exceeded slow threshold" in message:
        return "acquire_slow"
    if "pool timed out while waiting for an open connection" in message:
        return "pool_timeout"
    return "pool_wait"


def as_float(value: Any) -> float | None:
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def as_int(value: Any) -> int | None:
    return value if isinstance(value, int) else None


def iter_json_log_records(path: Path) -> tuple[list[dict[str, Any]], int]:
    records: list[dict[str, Any]] = []
    skipped = 0
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                skipped += 1
                continue
            if isinstance(parsed, dict):
                records.append(parsed)
            else:
                skipped += 1
    return records, skipped


def build_report(
    log_path: Path,
    start: datetime | None,
    end: datetime | None,
    min_elapsed_secs: float,
) -> SlowSqlReport:
    records, skipped = iter_json_log_records(log_path)
    slow_rows: list[SlowSqlRow] = []
    pool_rows: list[PoolWaitRow] = []
    for record in records:
        timestamp = record.get("timestamp")
        if not isinstance(timestamp, str):
            continue
        parsed_ts = parse_timestamp(timestamp)
        if parsed_ts is None:
            continue
        if start is not None and parsed_ts < start:
            continue
        if end is not None and parsed_ts > end:
            continue
        fields = record.get("fields")
        if not isinstance(fields, dict):
            continue
        message = fields.get("message")
        if not isinstance(message, str):
            continue
        if message == SLOW_SQL_MESSAGE:
            elapsed_secs = as_float(fields.get("elapsed_secs"))
            if elapsed_secs is None or elapsed_secs < min_elapsed_secs:
                continue
            slow_rows.append(
                SlowSqlRow(
                    timestamp=timestamp,
                    elapsed_secs=elapsed_secs,
                    summary=str(fields.get("summary") or ""),
                    rows_affected=as_int(fields.get("rows_affected")),
                    rows_returned=as_int(fields.get("rows_returned")),
                    request_id=span_value(record, "request_id"),
                    route=span_value(record, "http.route"),
                    statement=normalize_sql(str(fields.get("db.statement") or "")),
                )
            )
        elif any(pattern in message for pattern in POOL_WAIT_PATTERNS):
            pool_rows.append(
                PoolWaitRow(
                    timestamp=timestamp,
                    level=str(record.get("level") or ""),
                    target=str(record.get("target") or ""),
                    kind=pool_wait_kind(message),
                    acquire_after_secs=as_float(
                        fields.get("aquired_after_secs") or fields.get("acquired_after_secs")
                    ),
                    request_id=span_value(record, "request_id"),
                    route=span_value(record, "http.route"),
                    message=message,
                )
            )
    slow_rows.sort(key=lambda row: row.elapsed_secs, reverse=True)
    pool_rows.sort(key=lambda row: row.timestamp)
    return SlowSqlReport(
        slow_sql_rows=slow_rows,
        pool_wait_rows=pool_rows,
        json_records=len(records),
        skipped_lines=skipped,
        start=start.isoformat().replace("+00:00", "Z") if start else None,
        end=end.isoformat().replace("+00:00", "Z") if end else None,
    )


def write_report(report: SlowSqlReport, output_dir: Path, top: int) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    slow_path = output_dir / "slow-sql.tsv"
    counts_path = output_dir / "slow-sql-counts.tsv"
    pool_path = output_dir / "db-pool-waits.tsv"
    summary_path = output_dir / "slow-sql-summary.json"

    with slow_path.open("w", encoding="utf-8") as handle:
        for row in report.slow_sql_rows:
            handle.write(
                "\t".join(
                    [
                        row.timestamp,
                        f"{row.elapsed_secs:.9g}",
                        row.summary,
                        "" if row.rows_affected is None else str(row.rows_affected),
                        "" if row.rows_returned is None else str(row.rows_returned),
                        row.request_id,
                        row.route,
                        row.statement,
                    ]
                )
                + "\n"
            )

    counts = Counter(row.summary for row in report.slow_sql_rows)
    with counts_path.open("w", encoding="utf-8") as handle:
        for summary, count in counts.most_common():
            handle.write(f"{count}\t{summary}\n")

    with pool_path.open("w", encoding="utf-8") as handle:
        for row in report.pool_wait_rows:
            handle.write(
                "\t".join(
                    [
                        row.timestamp,
                        row.level,
                        row.target,
                        row.kind,
                        "" if row.acquire_after_secs is None else f"{row.acquire_after_secs:.9g}",
                        row.request_id,
                        row.route,
                        row.message,
                    ]
                )
                + "\n"
            )

    with summary_path.open("w", encoding="utf-8") as handle:
        json.dump(report.summary(top), handle, indent=2, sort_keys=True)
        handle.write("\n")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--log", type=Path, default=Path("api_server.log"))
    parser.add_argument("--start", help="inclusive UTC timestamp, e.g. 2026-07-03T05:53:39.971Z")
    parser.add_argument("--end", help="inclusive UTC timestamp, e.g. 2026-07-03T05:54:43.081Z")
    parser.add_argument("--min-elapsed-secs", type=float, default=0.0)
    parser.add_argument("--top", type=int, default=30)
    parser.add_argument("--output-dir", type=Path, required=True)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    if args.min_elapsed_secs < 0:
        print("--min-elapsed-secs must be non-negative", file=sys.stderr)
        return 2
    start = parse_timestamp(args.start)
    end = parse_timestamp(args.end)
    if args.start and start is None:
        print(f"invalid --start timestamp: {args.start}", file=sys.stderr)
        return 2
    if args.end and end is None:
        print(f"invalid --end timestamp: {args.end}", file=sys.stderr)
        return 2
    if start and end and start > end:
        print("--start must be <= --end", file=sys.stderr)
        return 2
    report = build_report(args.log, start, end, args.min_elapsed_secs)
    write_report(report, args.output_dir, args.top)
    print(json.dumps(report.summary(args.top), indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
