"""Rule and LLM-based checks for talk verification cases."""
from __future__ import annotations

import logging
import re
from dataclasses import dataclass
from typing import Any

from sqlalchemy import text

logger = logging.getLogger(__name__)


@dataclass
class CheckResult:
    name: str
    passed: bool
    message: str = ""
    score: float | None = None  # only for llm_judge


# ── Rule checks ──────────────────────────────────────────────────────

def check_tool_called(tool_name: str, tool_calls: list[dict]) -> CheckResult:
    names = [tc.get("name", "") for tc in tool_calls]
    found = any(tool_name in n for n in names)
    return CheckResult(
        f"tool_called:{tool_name}",
        found,
        f"called: {names}" if not found else "",
    )


def check_no_tool_called(tool_calls: list[dict]) -> CheckResult:
    # introspection is a read-only internal tool, not a side-effecting call
    _IGNORED = {"introspection"}
    significant = [tc for tc in tool_calls if tc.get("name") not in _IGNORED]
    return CheckResult(
        "no_tool_called",
        len(significant) == 0,
        f"unexpected tool calls: {[tc.get('name') for tc in significant]}" if significant else "",
    )


def check_response_contains(text_val: str, response: str) -> CheckResult:
    found = text_val.lower() in response.lower()
    return CheckResult(
        f"response_contains:{text_val}",
        found,
        "not found in response" if not found else "",
    )


def check_response_not_contains(text_val: str, response: str) -> CheckResult:
    found = text_val.lower() in response.lower()
    return CheckResult(
        f"response_not_contains:{text_val}",
        not found,
        "unexpectedly found in response" if found else "",
    )


def check_response_contains_any(values: list[str], response: str) -> CheckResult:
    found = any(v.lower() in response.lower() for v in values)
    return CheckResult(
        f"response_contains_any:{values}",
        found,
        f"none of {values} found in response" if not found else "",
    )


def check_db_rule(
    rule: dict,
    db_factory: Any,
    uid: str,
    sid: str,
    prev_counts: dict[str, int],
) -> list[CheckResult]:
    """Execute a db rule check, return list of CheckResults."""
    results = []
    table = rule["table"]
    where = rule["where"].replace(":uid", f"'{uid}'").replace(":sid", f"'{sid}'")
    asserts = rule.get("assert", {})

    with db_factory() as db:
        if "count" in asserts:
            actual = db.execute(text(f"SELECT COUNT(*) FROM {table} WHERE {where}")).scalar() or 0
            expected_str = str(asserts["count"])  # handle YAML int values
            key = f"{table}:{where}"

            if expected_str.startswith("+"):
                delta = int(expected_str[1:])
                prev = prev_counts.get(key, 0)
                ok = actual == prev + delta
                msg = f"count={actual}, expected {prev}+{delta}={prev+delta}"
            elif expected_str.startswith(">="):
                threshold = int(expected_str[2:])
                ok = actual >= threshold
                msg = f"count={actual}, expected >={threshold}"
            else:
                expected = int(expected_str)
                ok = actual == expected
                msg = f"count={actual}, expected {expected}"

            results.append(CheckResult(f"db:{table}:count", ok, msg))
            prev_counts[key] = actual

        if "fields" in asserts:
            row = db.execute(
                text(f"SELECT * FROM {table} WHERE {where} ORDER BY created_at DESC LIMIT 1")
            ).first()

            if row is None:
                results.append(CheckResult(f"db:{table}:fields", False, "no rows found"))
            else:
                row_dict = dict(row._mapping)
                for field_name, constraint in asserts["fields"].items():
                    if isinstance(constraint, dict):
                        if "contains" in constraint:
                            val = str(row_dict.get(field_name, ""))
                            substr = constraint["contains"]
                            ok = substr.lower() in val.lower()
                            results.append(CheckResult(
                                f"db:{table}:{field_name}:contains:{substr}",
                                ok,
                                f"value='{val[:100]}'" if not ok else "",
                            ))
                        if "not_null" in constraint:
                            val = row_dict.get(field_name)
                            ok = val is not None
                            results.append(CheckResult(
                                f"db:{table}:{field_name}:not_null",
                                ok,
                                "value is None" if not ok else "",
                            ))
                    else:
                        val = row_dict.get(field_name)
                        ok = val == constraint
                        results.append(CheckResult(
                            f"db:{table}:{field_name}={constraint}",
                            ok,
                            f"actual={val}" if not ok else "",
                        ))

    return results


def check_session_integrity(db_factory: Any, sid: str) -> list[CheckResult]:
    """Verify event chain structure and session record consistency."""
    results = []
    with db_factory() as db:
        events = db.execute(
            text(
                "SELECT event_type, session_id, parent_event_id, causal_chain_id "
                "FROM agent_events WHERE session_id = :sid ORDER BY created_at"
            ),
            {"sid": sid},
        ).fetchall()

        if not events:
            return [CheckResult("session_integrity:events_exist", False, "no events found")]

        # causal_chain_id not null on all events
        nulls = [e for e in events if not e.causal_chain_id]
        results.append(CheckResult(
            "session_integrity:causal_chain_id",
            len(nulls) == 0,
            f"{len(nulls)} events missing causal_chain_id" if nulls else "",
        ))

        # llm_response events have parent_event_id
        llm_events = [e for e in events if e.event_type == "llm_response"]
        missing_parent = [e for e in llm_events if not e.parent_event_id]
        results.append(CheckResult(
            "session_integrity:llm_response_has_parent",
            len(missing_parent) == 0,
            f"{len(missing_parent)} llm_response events missing parent_event_id" if missing_parent else "",
        ))

        # at least one llm_response exists
        results.append(CheckResult(
            "session_integrity:has_llm_response",
            len(llm_events) > 0,
            f"event types: {[e.event_type for e in events]}" if not llm_events else "",
        ))

        # session record: status=active, event_count matches
        session_row = db.execute(
            text("SELECT status, event_count FROM agent_sessions WHERE session_id = :sid"),
            {"sid": sid},
        ).first()
        if session_row:
            results.append(CheckResult(
                "session_integrity:status_active",
                session_row.status == "active",
                f"status={session_row.status}" if session_row.status != "active" else "",
            ))
            results.append(CheckResult(
                "session_integrity:event_count_positive",
                session_row.event_count > 0,
                f"event_count={session_row.event_count}" if session_row.event_count <= 0 else "",
            ))
        else:
            results.append(CheckResult(
                "session_integrity:session_record_exists", False, "no session record found"
            ))

    return results


def check_turn_count_increases(
    db_factory: Any, sid: str, prev_counts: dict[str, int]
) -> CheckResult:
    """Verify event count increased since last snapshot, then update snapshot."""
    key = f"__events:{sid}"
    prev = prev_counts.get(key, 0)
    with db_factory() as db:
        actual = db.execute(
            text("SELECT COUNT(*) FROM agent_events WHERE session_id = :sid"),
            {"sid": sid},
        ).scalar() or 0
    prev_counts[key] = actual
    increased = actual > prev
    return CheckResult(
        "turn_count_increases",
        increased,
        f"count={actual}, prev={prev}" if not increased else f"count={actual}",
    )


def run_rule_checks(
    rules: list[dict],
    response: str,
    tool_calls: list[dict],
    db_factory: Any,
    uid: str,
    sid: str,
    prev_counts: dict[str, int],
) -> list[CheckResult]:
    """Run all rule checks for a turn."""
    results = []
    for rule in rules:
        if "tool_called" in rule:
            results.append(check_tool_called(rule["tool_called"], tool_calls))
        elif "no_tool_called" in rule:
            results.append(check_no_tool_called(tool_calls))
        elif "response_contains" in rule:
            results.append(check_response_contains(rule["response_contains"], response))
        elif "response_not_contains" in rule:
            results.append(check_response_not_contains(rule["response_not_contains"], response))
        elif "response_contains_any" in rule:
            results.append(check_response_contains_any(rule["response_contains_any"], response))
        elif "session_integrity" in rule:
            results.extend(check_session_integrity(db_factory, sid))
        elif "turn_count_increases" in rule:
            results.append(check_turn_count_increases(db_factory, sid, prev_counts))
        elif "db" in rule:
            results.extend(check_db_rule(rule["db"], db_factory, uid, sid, prev_counts))
    return results


# ── LLM judge ────────────────────────────────────────────────────────

def llm_judge(
    response: str,
    user_message: str,
    criteria: str,
    pass_threshold: float,
    llm_client: Any,
    model: str | None = None,
) -> CheckResult:
    """Ask LLM to judge response quality, return score 0-1."""
    prompt = (
        "你是一个对话质量评判员。\n\n"
        f"用户说：{user_message}\n\n"
        f"Agent 回复：\n---\n{response}\n---\n\n"
        f"评判标准：{criteria}\n\n"
        "请给出 0 到 1 的分数（保留两位小数），只输出数字，不要解释。\n"
        "1.0 = 完全符合标准\n"
        "0.0 = 完全不符合"
    )
    try:
        from core.llm.client import LLMMessage
        messages = [LLMMessage(role="user", content=prompt)]
        resp = llm_client.chat(messages, user_id="__verify_judge", model=model)
        raw = resp.content if hasattr(resp, "content") else str(resp)
        match = re.search(r"(\d+\.?\d*)", raw.strip())
        if not match:
            import logging
            logging.getLogger(__name__).warning("llm_judge got non-numeric response: %r", raw[:200])
        score = float(match.group(1)) if match else 0.0
        score = min(1.0, max(0.0, score))
        passed = score >= pass_threshold
        return CheckResult(
            "llm_judge",
            passed,
            f"score={score:.2f} (threshold={pass_threshold})",
            score=score,
        )
    except Exception as e:
        return CheckResult("llm_judge", False, f"judge error: {e}", score=0.0)
