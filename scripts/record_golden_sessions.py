#!/usr/bin/env python3
"""Record real DeepSeek conversations as golden session fixtures.

Usage:
    python scripts/record_golden_sessions.py

Calls DeepSeek API with realistic multi-turn conversations, records every
event (user_query, llm_response, tool_call, tool_result) into the database,
then exports the session as a JSON fixture for offline regression testing.

The fixture can be replayed without any LLM API call via ToolMockingLayer.
"""

import json
import os
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from openai import OpenAI
from uuid_utils import uuid7

# ── Setup ─────────────────────────────────────────────────

PROJECT_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(PROJECT_ROOT))

DEEPSEEK_API_KEY = os.environ.get("DEEPSEEK_API_KEY")
if not DEEPSEEK_API_KEY:
    print("Error: Set DEEPSEEK_API_KEY environment variable")
    sys.exit(1)
DEEPSEEK_BASE_URL = "https://api.deepseek.com/v1"
MODEL = "deepseek-chat"

client = OpenAI(api_key=DEEPSEEK_API_KEY, base_url=DEEPSEEK_BASE_URL)

FIXTURE_DIR = PROJECT_ROOT / "tests" / "fixtures" / "golden_sessions"
FIXTURE_DIR.mkdir(parents=True, exist_ok=True)


def _uid():
    return str(uuid7())


def _now():
    return datetime.now(timezone.utc).isoformat()


def call_deepseek(messages: list[dict], temperature: float = 0.3) -> dict:
    """Call DeepSeek and return structured response with usage."""
    start = time.time()
    resp = client.chat.completions.create(
        model=MODEL,
        messages=messages,
        temperature=temperature,
        max_tokens=1024,
    )
    latency_ms = int((time.time() - start) * 1000)
    choice = resp.choices[0]
    return {
        "content": choice.message.content or "",
        "model": resp.model,
        "tokens_prompt": resp.usage.prompt_tokens,
        "tokens_completion": resp.usage.completion_tokens,
        "tokens_total": resp.usage.total_tokens,
        "latency_ms": latency_ms,
        "finish_reason": choice.finish_reason,
    }


# ── Scenario definitions ─────────────────────────────────


def scenario_code_review() -> dict:
    """Multi-turn: user asks for code review, LLM analyzes, user follows up."""
    session_id = _uid()
    user_id = "golden_user"
    chain_id = _uid()
    events = []

    # Turn 1: user asks for review
    user_content = (
        "Review this Python function and suggest improvements:\n\n"
        "```python\n"
        "def get_user(db, user_id):\n"
        "    result = db.execute(f\"SELECT * FROM auth_users WHERE id = '{user_id}'\")\n"
        "    rows = result.fetchall()\n"
        "    if len(rows) > 0:\n"
        "        return rows[0]\n"
        "    else:\n"
        "        return None\n"
        "```"
    )
    e1_id = _uid()
    events.append(
        {
            "event_id": e1_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": user_content,
            "causal_chain_id": chain_id,
            "parent_event_id": None,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 2: LLM responds
    messages = [
        {"role": "system", "content": "You are a senior Python code reviewer. Be concise."},
        {"role": "user", "content": user_content},
    ]
    llm1 = call_deepseek(messages)
    e2_id = _uid()
    events.append(
        {
            "event_id": e2_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm1["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e1_id,
            "created_at": _now(),
            "metadata": {"model": llm1["model"], "finish_reason": llm1["finish_reason"]},
            "token_usage": {
                "prompt": llm1["tokens_prompt"],
                "completion": llm1["tokens_completion"],
                "total": llm1["tokens_total"],
            },
            "llm_model_used": llm1["model"],
            "latency_ms": llm1["latency_ms"],
        }
    )

    # Turn 3: simulated tool_call (code_search to find similar patterns)
    e3_id = _uid()
    tool_params = {"query": "SQL injection f-string", "scope": "project"}
    events.append(
        {
            "event_id": e3_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "code_search",
            "skill_name": "code_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e2_id,
            "created_at": _now(),
            "metadata": {"skill_params": tool_params},
        }
    )

    # Turn 4: tool_result
    tool_result = {
        "data": "Found 3 instances of f-string SQL in: db_consumer.py:45, user_repo.py:23, event_repo.py:67",
        "source": "live",
    }
    e4_id = _uid()
    events.append(
        {
            "event_id": e4_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": tool_result["data"],
            "skill_name": "code_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e3_id,
            "created_at": _now(),
            "metadata": {
                "skill_params": tool_params,
                "skill_result": tool_result,
            },
        }
    )

    # Turn 5: user follow-up
    followup = "Can you write the fixed version using parameterized queries?"
    e5_id = _uid()
    events.append(
        {
            "event_id": e5_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": followup,
            "causal_chain_id": chain_id,
            "parent_event_id": e4_id,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 6: LLM responds with fix
    messages.append({"role": "assistant", "content": llm1["content"]})
    messages.append({"role": "user", "content": followup})
    llm2 = call_deepseek(messages)
    e6_id = _uid()
    events.append(
        {
            "event_id": e6_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm2["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e5_id,
            "created_at": _now(),
            "metadata": {"model": llm2["model"], "finish_reason": llm2["finish_reason"]},
            "token_usage": {
                "prompt": llm2["tokens_prompt"],
                "completion": llm2["tokens_completion"],
                "total": llm2["tokens_total"],
            },
            "llm_model_used": llm2["model"],
            "latency_ms": llm2["latency_ms"],
        }
    )

    return {
        "scenario": "code_review_sql_injection",
        "description": "Multi-turn code review: SQL injection detection and fix",
        "session_id": session_id,
        "user_id": user_id,
        "events": events,
        "recorded_at": _now(),
        "model": MODEL,
        "event_count": len(events),
    }


def scenario_debug_error() -> dict:
    """User pastes an error, LLM diagnoses, tool looks up docs, LLM synthesizes."""
    session_id = _uid()
    user_id = "golden_user"
    chain_id = _uid()
    events = []

    error_msg = (
        "I'm getting this error:\n\n"
        "```\n"
        "sqlalchemy.exc.OperationalError: (pymysql.err.OperationalError) "
        "(20101, \"internal error: Can't cast 'abc' to INT type\")\n"
        "```\n\n"
        "What's wrong?"
    )
    e1_id = _uid()
    events.append(
        {
            "event_id": e1_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": error_msg,
            "causal_chain_id": chain_id,
            "parent_event_id": None,
            "created_at": _now(),
            "metadata": {},
        }
    )

    messages = [
        {
            "role": "system",
            "content": "You are a database debugging expert. Be concise and actionable.",
        },
        {"role": "user", "content": error_msg},
    ]
    llm1 = call_deepseek(messages)
    e2_id = _uid()
    events.append(
        {
            "event_id": e2_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm1["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e1_id,
            "created_at": _now(),
            "metadata": {"model": llm1["model"]},
            "token_usage": {
                "prompt": llm1["tokens_prompt"],
                "completion": llm1["tokens_completion"],
                "total": llm1["tokens_total"],
            },
            "llm_model_used": llm1["model"],
            "latency_ms": llm1["latency_ms"],
        }
    )

    # Tool call: search docs
    e3_id = _uid()
    doc_params = {"query": "MatrixOne type cast error 20101"}
    events.append(
        {
            "event_id": e3_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "doc_search",
            "skill_name": "doc_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e2_id,
            "created_at": _now(),
            "metadata": {"skill_params": doc_params},
        }
    )

    doc_result = {
        "data": "Error 20101: Type mismatch in MatrixOne. Ensure column types match query parameters. "
        "Common cause: passing string to INT column without CAST().",
        "source": "live",
    }
    e4_id = _uid()
    events.append(
        {
            "event_id": e4_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": doc_result["data"],
            "skill_name": "doc_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e3_id,
            "created_at": _now(),
            "metadata": {"skill_params": doc_params, "skill_result": doc_result},
        }
    )

    # LLM synthesizes tool result
    messages.append({"role": "assistant", "content": llm1["content"]})
    messages.append(
        {
            "role": "user",
            "content": f"I found this in the docs: {doc_result['data']}\nGive me the specific fix.",
        }
    )
    llm2 = call_deepseek(messages)
    e5_id = _uid()
    events.append(
        {
            "event_id": e5_id,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm2["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e4_id,
            "created_at": _now(),
            "metadata": {"model": llm2["model"]},
            "token_usage": {
                "prompt": llm2["tokens_prompt"],
                "completion": llm2["tokens_completion"],
                "total": llm2["tokens_total"],
            },
            "llm_model_used": llm2["model"],
            "latency_ms": llm2["latency_ms"],
        }
    )

    return {
        "scenario": "debug_type_cast_error",
        "description": "Debug MatrixOne type cast error with doc search",
        "session_id": session_id,
        "user_id": user_id,
        "events": events,
        "recorded_at": _now(),
        "model": MODEL,
        "event_count": len(events),
    }


def scenario_chained_tool_calls() -> dict:
    """Chained tool calls: search → analyze → apply patch. 3 tools, 10 events.

    LLM decides next action based on previous tool result (causal dependency).
    """
    session_id = _uid()
    user_id = "golden_user"
    chain_id = _uid()
    events = []

    # Turn 1: user request
    user_content = (
        "There's a performance issue in our query pipeline. "
        "Find the slow queries, analyze them, and apply optimizations."
    )
    e1 = _uid()
    events.append(
        {
            "event_id": e1,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": user_content,
            "causal_chain_id": chain_id,
            "parent_event_id": None,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 2: LLM plans approach
    msgs = [
        {
            "role": "system",
            "content": "You are a database performance expert. Plan step by step, be concise.",
        },
        {"role": "user", "content": user_content},
    ]
    llm1 = call_deepseek(msgs)
    e2 = _uid()
    events.append(
        {
            "event_id": e2,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm1["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e1,
            "created_at": _now(),
            "metadata": {"model": llm1["model"]},
            "token_usage": {
                "prompt": llm1["tokens_prompt"],
                "completion": llm1["tokens_completion"],
                "total": llm1["tokens_total"],
            },
            "llm_model_used": llm1["model"],
            "latency_ms": llm1["latency_ms"],
        }
    )

    # Turn 3-4: tool_call → tool_result (slow_query_search)
    tc1_params = {"threshold_ms": 500, "limit": 10}
    e3 = _uid()
    events.append(
        {
            "event_id": e3,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "slow_query_search",
            "skill_name": "slow_query_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e2,
            "created_at": _now(),
            "metadata": {"skill_params": tc1_params},
        }
    )
    tr1_data = {
        "data": json.dumps(
            [
                {
                    "query": "SELECT * FROM events WHERE session_id = ?",
                    "avg_ms": 1200,
                    "calls": 500,
                },
                {
                    "query": "SELECT * FROM events ORDER BY created_at DESC LIMIT 100",
                    "avg_ms": 800,
                    "calls": 200,
                },
            ]
        ),
        "source": "live",
    }
    e4 = _uid()
    events.append(
        {
            "event_id": e4,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": tr1_data["data"],
            "skill_name": "slow_query_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e3,
            "created_at": _now(),
            "metadata": {"skill_params": tc1_params, "skill_result": tr1_data},
        }
    )

    # Turn 5: LLM analyzes results, decides to check indexes
    msgs.append({"role": "assistant", "content": llm1["content"]})
    msgs.append(
        {
            "role": "user",
            "content": f"Slow query results: {tr1_data['data']}\nAnalyze and suggest index changes.",
        }
    )
    llm2 = call_deepseek(msgs)
    e5 = _uid()
    events.append(
        {
            "event_id": e5,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm2["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e4,
            "created_at": _now(),
            "metadata": {"model": llm2["model"]},
            "token_usage": {
                "prompt": llm2["tokens_prompt"],
                "completion": llm2["tokens_completion"],
                "total": llm2["tokens_total"],
            },
            "llm_model_used": llm2["model"],
            "latency_ms": llm2["latency_ms"],
        }
    )

    # Turn 6-7: tool_call → tool_result (index_analyzer) — depends on Turn 4 results
    tc2_params = {"table": "events", "query_pattern": "SELECT * FROM events WHERE session_id = ?"}
    e6 = _uid()
    events.append(
        {
            "event_id": e6,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "index_analyzer",
            "skill_name": "index_analyzer",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e5,
            "created_at": _now(),
            "metadata": {"skill_params": tc2_params},
        }
    )
    tr2_data = {
        "data": json.dumps(
            {
                "table": "events",
                "existing_indexes": ["PRIMARY(event_id)", "idx_session_id(session_id)"],
                "recommendation": "Index idx_session_id exists but query uses SELECT * — add covering index or select specific columns",
            }
        ),
        "source": "live",
    }
    e7 = _uid()
    events.append(
        {
            "event_id": e7,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": tr2_data["data"],
            "skill_name": "index_analyzer",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e6,
            "created_at": _now(),
            "metadata": {"skill_params": tc2_params, "skill_result": tr2_data},
        }
    )

    # Turn 8-9: tool_call → tool_result (apply_optimization) — depends on Turn 7
    tc3_params = {
        "action": "rewrite_query",
        "original": "SELECT * FROM events WHERE session_id = ?",
        "optimized": "SELECT event_id, event_type, content, created_at FROM events WHERE session_id = ?",
    }
    e8 = _uid()
    events.append(
        {
            "event_id": e8,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "apply_optimization",
            "skill_name": "apply_optimization",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e7,
            "created_at": _now(),
            "metadata": {"skill_params": tc3_params},
        }
    )
    tr3_data = {"data": "Query rewritten. Estimated improvement: 1200ms → 150ms", "source": "live"}
    e9 = _uid()
    events.append(
        {
            "event_id": e9,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": tr3_data["data"],
            "skill_name": "apply_optimization",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e8,
            "created_at": _now(),
            "metadata": {"skill_params": tc3_params, "skill_result": tr3_data},
        }
    )

    # Turn 10: LLM final summary
    msgs.append({"role": "assistant", "content": llm2["content"]})
    msgs.append(
        {
            "role": "user",
            "content": f"Optimization applied: {tr3_data['data']}. Summarize what was done.",
        }
    )
    llm3 = call_deepseek(msgs)
    e10 = _uid()
    events.append(
        {
            "event_id": e10,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm3["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e9,
            "created_at": _now(),
            "metadata": {"model": llm3["model"]},
            "token_usage": {
                "prompt": llm3["tokens_prompt"],
                "completion": llm3["tokens_completion"],
                "total": llm3["tokens_total"],
            },
            "llm_model_used": llm3["model"],
            "latency_ms": llm3["latency_ms"],
        }
    )

    return {
        "scenario": "chained_perf_optimization",
        "description": "3-tool chain: slow_query_search → index_analyzer → apply_optimization",
        "session_id": session_id,
        "user_id": user_id,
        "events": events,
        "recorded_at": _now(),
        "model": MODEL,
        "event_count": len(events),
    }


def scenario_multi_turn_correction() -> dict:
    """User corrects LLM mistake. Tests: LLM gives wrong answer → user pushes back → LLM fixes."""
    session_id = _uid()
    user_id = "golden_user"
    chain_id = _uid()
    events = []

    # Turn 1
    q1 = "What's the default isolation level in MatrixOne?"
    e1 = _uid()
    events.append(
        {
            "event_id": e1,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": q1,
            "causal_chain_id": chain_id,
            "parent_event_id": None,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 2: LLM answers (may or may not be correct — that's the point)
    msgs = [
        {"role": "system", "content": "You are a database expert. Answer concisely."},
        {"role": "user", "content": q1},
    ]
    llm1 = call_deepseek(msgs)
    e2 = _uid()
    events.append(
        {
            "event_id": e2,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm1["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e1,
            "created_at": _now(),
            "metadata": {"model": llm1["model"]},
            "token_usage": {
                "prompt": llm1["tokens_prompt"],
                "completion": llm1["tokens_completion"],
                "total": llm1["tokens_total"],
            },
            "llm_model_used": llm1["model"],
            "latency_ms": llm1["latency_ms"],
        }
    )

    # Turn 3: user corrects / challenges
    correction = (
        "That's not quite right. MatrixOne uses snapshot isolation (SI) by default, "
        "not READ COMMITTED. It's based on TAE (Transactional Analytical Engine). "
        "Can you also explain how this affects our replay system's time-travel queries?"
    )
    e3 = _uid()
    events.append(
        {
            "event_id": e3,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": correction,
            "causal_chain_id": chain_id,
            "parent_event_id": e2,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 4: tool_call to verify
    tc_params = {"query": "MatrixOne snapshot isolation TAE time-travel"}
    e4 = _uid()
    events.append(
        {
            "event_id": e4,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_call",
            "content": "doc_search",
            "skill_name": "doc_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e3,
            "created_at": _now(),
            "metadata": {"skill_params": tc_params},
        }
    )
    tr_data = {
        "data": "MatrixOne uses Snapshot Isolation (SI) via TAE engine. "
        "Each transaction sees a consistent snapshot. "
        "Time-travel queries use SNAPSHOT syntax to read historical data at any point.",
        "source": "live",
    }
    e5 = _uid()
    events.append(
        {
            "event_id": e5,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "tool_result",
            "content": tr_data["data"],
            "skill_name": "doc_search",
            "skill_version": "1.0.0",
            "causal_chain_id": chain_id,
            "parent_event_id": e4,
            "created_at": _now(),
            "metadata": {"skill_params": tc_params, "skill_result": tr_data},
        }
    )

    # Turn 5: LLM corrects itself with doc evidence
    msgs.append({"role": "assistant", "content": llm1["content"]})
    msgs.append({"role": "user", "content": correction})
    msgs.append({"role": "user", "content": f"Documentation says: {tr_data['data']}"})
    llm2 = call_deepseek(msgs)
    e6 = _uid()
    events.append(
        {
            "event_id": e6,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm2["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e5,
            "created_at": _now(),
            "metadata": {"model": llm2["model"]},
            "token_usage": {
                "prompt": llm2["tokens_prompt"],
                "completion": llm2["tokens_completion"],
                "total": llm2["tokens_total"],
            },
            "llm_model_used": llm2["model"],
            "latency_ms": llm2["latency_ms"],
        }
    )

    # Turn 6: user asks deeper question
    q3 = "So if I create a SNAPSHOT checkpoint before running regression tests, can I guarantee the test sees exactly the same data as production did at that moment?"
    e7 = _uid()
    events.append(
        {
            "event_id": e7,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "user_query",
            "content": q3,
            "causal_chain_id": chain_id,
            "parent_event_id": e6,
            "created_at": _now(),
            "metadata": {},
        }
    )

    # Turn 7: LLM final answer
    msgs.append({"role": "assistant", "content": llm2["content"]})
    msgs.append({"role": "user", "content": q3})
    llm3 = call_deepseek(msgs)
    e8 = _uid()
    events.append(
        {
            "event_id": e8,
            "session_id": session_id,
            "user_id": user_id,
            "event_type": "llm_response",
            "content": llm3["content"],
            "causal_chain_id": chain_id,
            "parent_event_id": e7,
            "created_at": _now(),
            "metadata": {"model": llm3["model"]},
            "token_usage": {
                "prompt": llm3["tokens_prompt"],
                "completion": llm3["tokens_completion"],
                "total": llm3["tokens_total"],
            },
            "llm_model_used": llm3["model"],
            "latency_ms": llm3["latency_ms"],
        }
    )

    return {
        "scenario": "multi_turn_correction",
        "description": "User corrects LLM on MatrixOne isolation level, LLM verifies via doc_search and self-corrects",
        "session_id": session_id,
        "user_id": user_id,
        "events": events,
        "recorded_at": _now(),
        "model": MODEL,
        "event_count": len(events),
    }


# ── Main ──────────────────────────────────────────────────


def main():
    scenarios = [
        ("code_review", scenario_code_review),
        ("debug_error", scenario_debug_error),
        ("chained_tool_calls", scenario_chained_tool_calls),
        ("multi_turn_correction", scenario_multi_turn_correction),
    ]

    for name, fn in scenarios:
        print(f"Recording scenario: {name} ...")
        try:
            data = fn()
            path = FIXTURE_DIR / f"{name}.json"
            path.write_text(json.dumps(data, indent=2, ensure_ascii=False))
            print(f"  ✅ {data['event_count']} events → {path}")
        except Exception as e:
            print(f"  ❌ Failed: {e}")
            import traceback

            traceback.print_exc()

    print("\nDone. Fixtures saved to tests/fixtures/golden_sessions/")


if __name__ == "__main__":
    main()
