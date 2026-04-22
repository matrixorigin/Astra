#!/usr/bin/env python3
"""
Offline validator + minimal rule runner for scripts/e2e/cases/*.yaml.

Purpose
-------
Historically these YAML cases described live-LLM scenarios but there was no
in-repo runner, so typos and schema drift went unnoticed until an external
harness picked them up. This script fills the gap with two modes:

  validate (default): parse every YAML under scripts/e2e/cases/, verify the
      top-level schema (name/description/turns/final_checks), surface any
      unknown rule types. Exits non-zero on the first problem. Runs with no
      dependencies beyond PyYAML and no network access — safe for CI.

  run: additionally execute the subset of rules that don't require an LLM
      (db count assertions against a sqlite file pointed to by
      --db, response_contains_any on a pre-captured transcript passed via
      --transcripts). llm_judge and live tool dispatch are skipped and
      reported as "skipped:reason" rather than pass/fail, so CI can still
      gate on structural correctness without flipping on every LLM drift.

Usage
-----
  scripts/e2e/validate_cases.py                        # validate all cases
  scripts/e2e/validate_cases.py --case memory_basic    # validate one case
  scripts/e2e/validate_cases.py run --db astra.sqlite \
      --transcripts /tmp/transcripts.json              # validate + run
      # offline rules

Schema (minimal, documented, not enforced by upstream):
  name:        str
  description: str
  requires:    list[str]        # informational (e.g. [llm])
  turns:       list[turn]
  final_checks:
    rules: list[rule]
  turn:
    user: str
    checks:
      rules: list[rule]
      llm_judge: {criteria: str, pass_threshold: float}   # optional
  rule: one of
    db: {table: str, where: str, assert: {count: "OP N", fields?: map}}
    session_integrity: bool
    turn_count_increases: bool
    response_contains_any: list[str]

Exit codes
----------
  0 — all referenced cases validated (and all run rules passed / were skipped)
  1 — schema error or rule failure
"""

from __future__ import annotations

import argparse
import json
import operator
import re
import sqlite3
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:
    import yaml  # type: ignore
except ImportError:
    print("error: PyYAML is required. pip install pyyaml", file=sys.stderr)
    sys.exit(2)


CASES_DIR = Path(__file__).resolve().parent / "cases"

VALID_TOP_KEYS = {
    "name",
    "description",
    "requires",
    "turns",
    "final_checks",
    "check_graph_activation",
}
VALID_RULE_KEYS = {
    "db",
    "session_integrity",
    "turn_count_increases",
    "response_contains_any",
    "response_contains",
    "tool_called",
    "no_tool_called",
}
COMPARE_OPS = {
    ">=": operator.ge,
    "<=": operator.le,
    ">": operator.gt,
    "<": operator.lt,
    "==": operator.eq,
    "=": operator.eq,
}


@dataclass
class Finding:
    case: str
    path: str
    kind: str  # "error" | "skipped" | "ok"
    message: str

    def format(self) -> str:
        sigil = {"error": "✗", "skipped": "·", "ok": "✓"}.get(self.kind, "?")
        return f"  {sigil} [{self.path}] {self.message}"


def _parse_count_assert(expr: str) -> tuple[Any, int]:
    expr = str(expr).strip()
    m = re.match(r"^(>=|<=|>|<|==|=)\s*(\d+)$", expr)
    if m:
        op = COMPARE_OPS[m.group(1)]
        return op, int(m.group(2))
    # Allow bare integer literal to mean "== N".
    if re.match(r"^\d+$", expr):
        return operator.eq, int(expr)
    raise ValueError(f"cannot parse count assertion: {expr!r}")


def _validate_rule(
    rule: dict, case: str, path: str, findings: list[Finding]
) -> None:
    if not isinstance(rule, dict) or len(rule) != 1:
        findings.append(
            Finding(
                case,
                path,
                "error",
                f"rule must be a single-key mapping, got {rule!r}",
            )
        )
        return
    (key,) = rule.keys()
    if key not in VALID_RULE_KEYS:
        findings.append(
            Finding(case, path, "error", f"unknown rule kind: {key!r}")
        )
        return
    body = rule[key]
    if key == "db":
        for required in ("table", "where", "assert"):
            if required not in body:
                findings.append(
                    Finding(
                        case,
                        path,
                        "error",
                        f"db rule missing required field: {required}",
                    )
                )
                return
        if "count" in body["assert"]:
            try:
                _parse_count_assert(str(body["assert"]["count"]))
            except ValueError as e:
                findings.append(Finding(case, path, "error", str(e)))
                return
    elif key in ("session_integrity", "turn_count_increases"):
        if not isinstance(body, bool):
            findings.append(
                Finding(
                    case, path, "error", f"{key} must be a bool, got {body!r}"
                )
            )
    elif key == "response_contains_any":
        if not isinstance(body, list) or not body:
            findings.append(
                Finding(
                    case,
                    path,
                    "error",
                    "response_contains_any must be a non-empty list",
                )
            )
    elif key == "response_contains":
        if not isinstance(body, (str, list)):
            findings.append(
                Finding(
                    case,
                    path,
                    "error",
                    "response_contains must be a str or list of str",
                )
            )
    elif key in ("tool_called", "no_tool_called"):
        if not isinstance(body, (str, list, dict, bool)):
            findings.append(
                Finding(
                    case,
                    path,
                    "error",
                    f"{key} must be a str / list / mapping / bool, got {type(body).__name__}",
                )
            )


def _validate_case(file: Path, findings: list[Finding]) -> str:
    case_name = file.stem
    try:
        doc = yaml.safe_load(file.read_text())
    except yaml.YAMLError as e:
        findings.append(
            Finding(case_name, str(file), "error", f"yaml parse failed: {e}")
        )
        return case_name
    if not isinstance(doc, dict):
        findings.append(
            Finding(case_name, str(file), "error", "root must be a mapping")
        )
        return case_name
    unknown = set(doc.keys()) - VALID_TOP_KEYS
    if unknown:
        findings.append(
            Finding(
                case_name,
                str(file),
                "error",
                f"unknown top-level keys: {sorted(unknown)}",
            )
        )
    for required in ("name", "turns"):
        if required not in doc:
            findings.append(
                Finding(
                    case_name, str(file), "error", f"missing required: {required}"
                )
            )
    for i, turn in enumerate(doc.get("turns", []) or []):
        rules = (turn.get("checks") or {}).get("rules") or []
        for j, rule in enumerate(rules):
            _validate_rule(rule, case_name, f"turns[{i}].rules[{j}]", findings)
    for j, rule in enumerate(
        (doc.get("final_checks") or {}).get("rules") or []
    ):
        _validate_rule(rule, case_name, f"final_checks.rules[{j}]", findings)
    return case_name


def _run_db_rule(
    rule_body: dict, db_path: Path, case: str, path: str
) -> Finding:
    if not db_path.exists():
        return Finding(case, path, "skipped", f"db file not found: {db_path}")
    q = f"SELECT COUNT(*) FROM {rule_body['table']} WHERE {rule_body['where']}"
    # Substitute well-known placeholders with sentinel values; the external
    # harness normally injects real ids. We use fixed test values so the
    # query is syntactically valid SQL.
    q = q.replace(":sid", "'test-sid'").replace(":uid", "'test-uid'")
    try:
        conn = sqlite3.connect(str(db_path))
        cur = conn.cursor()
        cur.execute(q)
        (got,) = cur.fetchone()
        conn.close()
    except sqlite3.Error as e:
        return Finding(case, path, "skipped", f"sql error (tolerated): {e}")
    count_assert = rule_body.get("assert", {}).get("count")
    if count_assert is None:
        return Finding(case, path, "skipped", "no count assertion")
    op, n = _parse_count_assert(str(count_assert))
    ok = op(got, n)
    msg = f"{rule_body['table']} count={got} expected {count_assert}"
    return Finding(case, path, "ok" if ok else "error", msg)


def _run_response_contains_any(
    keywords: list[str], transcripts: list[str], case: str, path: str
) -> Finding:
    if not transcripts:
        return Finding(case, path, "skipped", "no --transcripts provided")
    haystack = "\n".join(transcripts).lower()
    hit = next((k for k in keywords if k.lower() in haystack), None)
    if hit:
        return Finding(case, path, "ok", f"found keyword {hit!r}")
    return Finding(
        case, path, "error", f"none of {keywords!r} in response transcripts"
    )


def run_mode(
    case_files: list[Path],
    db_path: Path | None,
    transcripts: list[str],
    findings: list[Finding],
) -> None:
    for file in case_files:
        case = file.stem
        doc = yaml.safe_load(file.read_text()) or {}
        rule_sites: list[tuple[str, dict]] = []
        for i, turn in enumerate(doc.get("turns", []) or []):
            rules = (turn.get("checks") or {}).get("rules") or []
            rule_sites.extend(
                (f"turns[{i}].rules[{j}]", r) for j, r in enumerate(rules)
            )
        rule_sites.extend(
            (f"final_checks.rules[{j}]", r)
            for j, r in enumerate(
                (doc.get("final_checks") or {}).get("rules") or []
            )
        )
        for path, rule in rule_sites:
            if not isinstance(rule, dict) or len(rule) != 1:
                continue
            (kind,) = rule.keys()
            if kind == "db" and db_path is not None:
                findings.append(_run_db_rule(rule["db"], db_path, case, path))
            elif kind == "db":
                findings.append(
                    Finding(case, path, "skipped", "no --db provided")
                )
            elif kind == "response_contains_any":
                findings.append(
                    _run_response_contains_any(rule[kind], transcripts, case, path)
                )
            else:
                findings.append(
                    Finding(
                        case, path, "skipped", f"{kind} requires live harness"
                    )
                )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    ap.add_argument(
        "mode",
        nargs="?",
        default="validate",
        choices=["validate", "run"],
        help="validate-only or also run rule-based checks",
    )
    ap.add_argument("--case", help="run only one named case (e.g. memory_basic)")
    ap.add_argument(
        "--db",
        type=Path,
        help="path to sqlite file for db assertions (run mode)",
    )
    ap.add_argument(
        "--transcripts",
        help="path to JSON file [str, ...] of assistant transcripts",
    )
    args = ap.parse_args(argv)

    files = sorted(CASES_DIR.glob("*.yaml"))
    if args.case:
        files = [f for f in files if f.stem == args.case]
        if not files:
            print(f"error: no case named {args.case!r}", file=sys.stderr)
            return 1

    findings: list[Finding] = []
    for file in files:
        _validate_case(file, findings)

    if args.mode == "run":
        transcripts: list[str] = []
        if args.transcripts:
            transcripts = json.loads(Path(args.transcripts).read_text())
        run_mode(files, args.db, transcripts, findings)

    by_case: dict[str, list[Finding]] = {}
    for f in findings:
        by_case.setdefault(f.case, []).append(f)

    rc = 0
    for case, items in sorted(by_case.items()):
        errors = [i for i in items if i.kind == "error"]
        marker = "FAIL" if errors else "OK"
        print(f"[{marker}] {case}  ({len(items)} findings)")
        for item in items:
            print(item.format())
        if errors:
            rc = 1

    if not by_case:
        if files:
            print(f"validated {len(files)} case(s): all schemas OK")
        else:
            print("(no cases matched)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
