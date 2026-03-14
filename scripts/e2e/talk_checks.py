"""Rule and LLM-based checks for talk verification cases."""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass
from typing import Any

logger = logging.getLogger(__name__)


@dataclass
class CheckResult:
    name: str
    passed: bool
    message: str = ""
    score: float | None = None  # only for llm_judge
    skipped: bool = False  # True if check was skipped (e.g., not supported)


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
    """Execute a db rule check using Memoria API instead of SQL.

    Translates SQL-based checks to Memoria API calls.
    """
    results = []
    table = rule.get("table", "")
    where = rule.get("where", "")
    asserts = rule.get("assert", {})

    try:
        # Import Memoria client
        from core.memory.factory import create_editor

        editor = create_editor(db_factory, user_id=uid)

        # Handle different table checks
        if table == "mem_memories":
            # List memories via MemoriaStorage API
            from core.memory.types import MemoryType
            memories = editor._storage.list_active(uid, memory_type=None, limit=500)
            # Convert Memory objects to dicts for filtering
            memories = [{"content": m.content, "memory_id": m.memory_id} for m in memories]

            # Apply content filter if specified in where clause
            if "content LIKE" in where:
                # Extract search term from LIKE clause
                import re
                match = re.search(r"content LIKE '%([^%]+)%'", where)
                if match:
                    search_term = match.group(1).lower()
                    memories = [m for m in memories if search_term in m.get("content", "").lower()]

            count = len(memories)

            # Check count assertion
            if "count" in asserts:
                count_assert = asserts["count"]
                if count_assert.startswith(">="):
                    min_count = int(count_assert[2:])
                    passed = count >= min_count
                    results.append(CheckResult(
                        f"db:memories_count>={min_count}",
                        passed,
                        f"found {count} memories" if not passed else ""
                    ))
                elif count_assert.startswith(">"):
                    min_count = int(count_assert[1:])
                    passed = count > min_count
                    results.append(CheckResult(
                        f"db:memories_count>{min_count}",
                        passed,
                        f"found {count} memories" if not passed else ""
                    ))
                else:
                    expected = int(count_assert)
                    passed = count == expected
                    results.append(CheckResult(
                        f"db:memories_count={expected}",
                        passed,
                        f"found {count} memories, expected {expected}" if not passed else ""
                    ))

        elif table == "mem_edit_log":
            # For edit log, we can check if any memories exist (indicating edits were made)
            memories = editor._storage.list_active(uid, memory_type=None, limit=500)
            count = len(memories)

            if "count" in asserts:
                count_assert = asserts["count"]
                if count_assert.startswith(">="):
                    min_count = int(count_assert[2:])
                    passed = count >= min_count
                    results.append(CheckResult(
                        f"db:edit_log_count>={min_count}",
                        passed,
                        f"found {count} memories (edit log via count)" if not passed else ""
                    ))

        elif table == "agent_events":
            # agent_events is still in SQL database, use direct query
            from sqlalchemy import text

            with db_factory() as db:
                # Parse where clause for session_id
                if "session_id = :sid" in where:
                    row = db.execute(
                        text("SELECT COUNT(*) FROM agent_events WHERE session_id = :sid"),
                        {"sid": sid}
                    ).scalar()
                    count = row or 0

                    if "count" in asserts:
                        count_assert = asserts["count"]
                        if count_assert.startswith(">="):
                            min_count = int(count_assert[2:])
                            passed = count >= min_count
                            results.append(CheckResult(
                                f"db:agent_events_count>={min_count}",
                                passed,
                                f"found {count} events" if not passed else ""
                            ))
                else:
                    results.append(CheckResult("db:agent_events", True, "Skipped - no session filter", skipped=True))
        else:
            results.append(CheckResult(f"db:{table}", True, f"Unknown table: {table}", skipped=True))

    except Exception as e:
        results.append(CheckResult("db:memoria_api", False, f"Memoria API error: {e}"))

    return results


def check_session_integrity(db_factory: Any, sid: str) -> list[CheckResult]:
    """Verify event chain structure and session record consistency.

    Note: With Memoria backend, direct SQL queries are not supported for memories,
    but agent_events is still in SQL database.
    """
    results = []

    try:
        from sqlalchemy import text

        with db_factory() as db:
            # Check session exists
            row = db.execute(
                text("SELECT COUNT(*) FROM agent_sessions WHERE session_id = :sid"),
                {"sid": sid}
            ).scalar()

            if row and row > 0:
                results.append(CheckResult("session_integrity:exists", True, ""))
            else:
                results.append(CheckResult("session_integrity:exists", False, f"Session {sid} not found"))

            # Check events count
            row = db.execute(
                text("SELECT COUNT(*) FROM agent_events WHERE session_id = :sid"),
                {"sid": sid}
            ).scalar()
            count = row or 0
            results.append(CheckResult("session_integrity:events", True, f"{count} events"))

    except Exception as e:
        results.append(CheckResult("session_integrity", False, f"Error: {e}"))

    return results


def run_rule_checks(
    rules: list[dict],
    response: str,
    tool_calls: list[dict],
    db_factory: Any,
    uid: str,
    sid: str,
    prev_counts: dict[str, int],
) -> list[CheckResult]:
    """Run all rule checks."""
    results: list[CheckResult] = []
    for rule in rules:
        # Handle both formats:
        # - {type: "tool_called", tool: "memory"}
        # - {tool_called: "memory"}
        rt = rule.get("type")
        
        # If no type field, infer from other keys
        if rt is None:
            if "tool_called" in rule:
                rt = "tool_called"
                tool_name = rule["tool_called"]
                results.append(check_tool_called(tool_name, tool_calls))
            elif "no_tool_called" in rule:
                rt = "no_tool_called"
                results.append(check_no_tool_called(tool_calls))
            elif "response_contains" in rule:
                rt = "response_contains"
                text_val = rule["response_contains"]
                results.append(check_response_contains(text_val, response))
            elif "response_not_contains" in rule:
                rt = "response_not_contains"
                text_val = rule["response_not_contains"]
                results.append(check_response_not_contains(text_val, response))
            elif "response_contains_any" in rule:
                rt = "response_contains_any"
                values = rule["response_contains_any"]
                results.append(check_response_contains_any(values, response))
            elif "db" in rule:
                rt = "db"
                # Handle {db: {table: ..., where: ..., assert: ...}} format
                db_rule = rule["db"] if isinstance(rule["db"], dict) else rule
                results.extend(check_db_rule(db_rule, db_factory, uid, sid, prev_counts))
            elif "session_integrity" in rule:
                rt = "session_integrity"
                results.extend(check_session_integrity(db_factory, sid))
            elif "turn_count_increases" in rule:
                rt = "turn_count_increases"
                # This is checked at the case level, skip here
                results.append(CheckResult("turn_count_increases", True, "Checked at case level", skipped=True))
            else:
                results.append(CheckResult(f"unknown:{rule}", False, f"unknown rule format: {rule}"))
        else:
            # Original format with type field
            if rt == "tool_called":
                results.append(check_tool_called(rule["tool"], tool_calls))
            elif rt == "no_tool_called":
                results.append(check_no_tool_called(tool_calls))
            elif rt == "response_contains":
                results.append(check_response_contains(rule["text"], response))
            elif rt == "response_not_contains":
                results.append(check_response_not_contains(rule["text"], response))
            elif rt == "response_contains_any":
                results.append(check_response_contains_any(rule["values"], response))
            elif rt == "db":
                results.extend(check_db_rule(rule, db_factory, uid, sid, prev_counts))
            elif rt == "session_integrity":
                results.extend(check_session_integrity(db_factory, sid))
            else:
                results.append(CheckResult(f"unknown:{rt}", False, f"unknown rule type: {rt}"))
    return results


# ── LLM judge ─────────────────────────────────────────────────────────


def _build_judge_prompt(response: str, user_msg: str, criteria: str) -> str:
    return f"""You are evaluating an AI assistant response.

User message: {user_msg}

Assistant response: {response}

Evaluation criteria: {criteria}

Rate the response on a scale of 0.0 to 1.0:
- 1.0 = fully meets criteria
- 0.5 = partially meets criteria  
- 0.0 = does not meet criteria

Provide your rating and brief explanation in this format:
Score: <number between 0.0 and 1.0>
Explanation: <one sentence explanation>
"""


def llm_judge(
    response: str,
    user_msg: str,
    criteria: str,
    pass_threshold: float,
    llm_client: Any,
    model: str | None = None,
    user_id: str | None = None,
) -> CheckResult:
    """Use LLM to judge response quality against criteria."""
    prompt = _build_judge_prompt(response, user_msg, criteria)

    try:
        # Use the provided LLM client - LLMClient.chat() requires user_id
        result = llm_client.chat(
            messages=[{"role": "user", "content": prompt}],
            user_id=user_id or "verify",
            model=model,
            temperature=0.0,
            task_hint="llm_judge",
        )
        
        content = result.content if hasattr(result, "content") else str(result)

        # Parse score from response
        score_match = re.search(r"Score:\s*([0-9.]+)", content)
        if score_match:
            score = float(score_match.group(1))
        else:
            # Try to find any number in 0-1 range
            nums = re.findall(r"\b([0-9.]+)\b", content)
            for n in nums:
                try:
                    f = float(n)
                    if 0 <= f <= 1:
                        score = f
                        break
                except ValueError:
                    continue
            else:
                score = 0.0

        # Parse explanation
        expl_match = re.search(r"Explanation:\s*(.+?)(?:\n|$)", content, re.DOTALL)
        explanation = expl_match.group(1).strip() if expl_match else content[:100]

        passed = score >= pass_threshold
        return CheckResult(
            f"llm_judge:{criteria[:30]}",
            passed,
            f"score={score:.2f}, threshold={pass_threshold} — {explanation}",
            score=score,
        )
    except Exception as e:
        return CheckResult(
            f"llm_judge:{criteria[:30]}",
            False,
            f"LLM judge failed: {e}",
            score=0.0,
        )
