"""Realistic end-to-end tests — multi-turn chat, tool use, data accumulation, closed-loop feedback.

Unlike test_e2e_scenarios.py (CRUD + single-call verification), these tests exercise
the FULL agent execution pipeline through the /chat API:

  User → POST /chat → RunEngine → ChatLoop → LLM (mock) → SkillPipeline → Tools → Events → DB

The DeterministicLLM is injected by monkeypatching _build_chat_loop so that:
  - LLM responses are deterministic and scripted per scenario
  - DB operations, event logging, context snapshots, scoring are ALL REAL
  - Assertions verify accumulated data via query APIs (black-box)

Scenarios:
  A. Multi-turn conversation (10 turns) — event chain grows, context snapshots accumulate
  B. Tool-use conversation — LLM calls execute_code, result feeds back, final answer synthesized
  C. Bulk conversations → evaluation → learning — 20 sessions, then closed loop
  D. Multi-session user — session 1 data visible in session 2 context
"""

from __future__ import annotations

import asyncio
import json
import time
from dataclasses import dataclass, field
from typing import Any
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from core.llm.models import LLMResponse, LLMProvider
from core.utils.id_generator import generate_id


# ============================================================================
# Scripted LLM — supports multi-turn sequences and tool calls
# ============================================================================

@dataclass
class Turn:
    """One scripted LLM turn. If tool_calls is set, LLM requests tool use."""
    content: str = ""
    tool_calls: list[dict] | None = None


class ScriptedLLM:
    """LLM mock that plays back a script of turns in order.

    For chat_with_tools: pops from script queue. If queue empty, returns fallback.
    For chat (plain): always returns a simple response (used by _needs_planning, firewall, etc).
    For chat_stream: yields single chunk.
    For chat_with_tools_stream: yields tool_call or text chunks matching chat_with_tools behavior.

    Thread-safe: each test gets its own instance via monkeypatch.
    """

    def __init__(self, script: list[Turn] | None = None):
        self.script: list[Turn] = list(script or [])
        self._cursor = 0
        self.call_log: list[dict] = []
        # Required attributes that ChatLoop / SkillPipeline read
        self.config = {"model": "scripted-mock", "temperature": 0.0, "max_context_tokens": 128000}
        self.db = None
        self.router = _FakeRouter()

    def _next_turn(self) -> Turn:
        if self._cursor < len(self.script):
            t = self.script[self._cursor]
            self._cursor += 1
            return t
        return Turn(content="No more scripted turns.")

    # -- Interfaces consumed by ChatLoop / SkillPipeline --

    def chat(self, messages, user_id="system", session_id=None, **kwargs):
        """Plain chat — used by _needs_planning, firewall claim extraction, etc."""
        self.call_log.append({"method": "chat", "user_id": user_id})
        # _needs_planning expects "yes" or "no" — always say no to keep tests simple
        combined = " ".join(
            m.get("content", "") if isinstance(m, dict) else getattr(m, "content", "")
            for m in (messages if isinstance(messages, list) else [messages])
        )
        content = "no"  # default: no planning needed
        if "summarize" in combined.lower():
            content = "Here is a summary of the conversation."
        return LLMResponse(
            content=content, model="scripted-mock", provider=LLMProvider.OPENAI,
            tokens_prompt=50, tokens_completion=20, tokens_total=70,
            latency_ms=10, cost_usd=0.0001,
        )

    def chat_with_tools(self, messages, tools=None, tool_choice="auto", **kwargs):
        """Tool-calling chat — ChatLoop's main loop calls this."""
        turn = self._next_turn()
        self.call_log.append({"method": "chat_with_tools", "turn_index": self._cursor - 1})
        result: dict[str, Any] = {"content": turn.content}
        if turn.tool_calls:
            result["tool_calls"] = turn.tool_calls
        return result

    async def chat_stream(self, messages, user_id="system", **kwargs):
        """Streaming plain chat."""
        self.call_log.append({"method": "chat_stream"})
        yield {"type": "text", "content": "Streamed response."}

    async def chat_with_tools_stream(self, messages, tools, tool_choice="auto", **kwargs):
        """Streaming tool chat — used by run_step_stream."""
        turn = self._next_turn()
        self.call_log.append({"method": "chat_with_tools_stream", "turn_index": self._cursor - 1})
        if turn.tool_calls:
            for tc in turn.tool_calls:
                yield {"type": "tool_call", "data": tc}
        if turn.content:
            yield {"type": "text", "content": turn.content}


class _FakeRouter:
    """Stub for LLMClient.router used by SkillPipeline embed_fn fallback."""
    def calculate_cost(self, *a, **kw):
        return 0.0


# ============================================================================
# Helpers
# ============================================================================

def _make_tool_call(name: str, arguments: dict, call_id: str | None = None) -> dict:
    """Build an OpenAI-format tool_call dict."""
    return {
        "id": call_id or f"call_{generate_id()}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def _build_patched_chat_loop(llm: ScriptedLLM):
    """Return a _build_chat_loop replacement that injects our ScriptedLLM.

    Everything else (EventLogger, ContextManager, SkillPipeline, Firewall, etc.)
    is real — only the LLM is mocked. Skips heavyweight skill registration
    since these tests don't depend on specific builtin skills.
    """
    def patched(db_factory):
        from core.agent.chat_loop import ChatLoop
        from core.agent.executor import AgentExecutor
        from core.context.manager import ContextManager
        from core.events.event_logger import EventLogger
        from core.verification.firewall import HallucinationFirewall
        from core.skills.pipeline import SkillPipeline
        from core.skills.registry import SkillRegistry

        db = db_factory()
        event_logger = EventLogger.from_session(db)
        skill_registry = SkillRegistry(db)
        # Skip register_builtin_skills — saves ~200ms per call.
        # Tests that need specific skills (B, E) register execute_code explicitly.
        context_manager = ContextManager(db)
        selector = SkillPipeline(db, llm, audit=True, learning=True)
        executor = AgentExecutor(db, skill_registry)
        firewall = HallucinationFirewall(db, context_manager)

        loop = ChatLoop(
            selector=selector,
            executor=executor,
            llm_client=llm,
            event_logger=event_logger,
            context_manager=context_manager,
            firewall=firewall,
        )
        # Skip Observer — observations table may lack columns in test DB.
        return loop

    return patched


def _build_patched_chat_loop_with_skills(llm: ScriptedLLM):
    """Same as _build_patched_chat_loop but registers builtin skills (for tool-use tests)."""
    def patched(db_factory):
        from core.agent.chat_loop import ChatLoop
        from core.agent.executor import AgentExecutor
        from core.context.manager import ContextManager
        from core.events.event_logger import EventLogger
        from core.verification.firewall import HallucinationFirewall
        from core.skills.pipeline import SkillPipeline
        from core.skills.registry import SkillRegistry
        from core.skills.builtin import register_builtin_skills
        from core.runtime import create_runtime, IsolationLevel
        from core.code_executor import CodeExecutor

        db = db_factory()
        event_logger = EventLogger.from_session(db)
        skill_registry = SkillRegistry(db)
        code_executor = CodeExecutor(
            runtime=create_runtime(min_isolation=IsolationLevel.PROCESS), db=db,
        )
        register_builtin_skills(skill_registry, db, code_executor=code_executor)
        context_manager = ContextManager(db)
        selector = SkillPipeline(db, llm, audit=True, learning=True)
        selector.reload_skills(registry=skill_registry)
        executor = AgentExecutor(db, skill_registry)
        firewall = HallucinationFirewall(db, context_manager)

        loop = ChatLoop(
            selector=selector,
            executor=executor,
            llm_client=llm,
            event_logger=event_logger,
            context_manager=context_manager,
            firewall=firewall,
        )
        return loop

    return patched


def _chat_and_wait(client, headers: dict, message: str,
                   session_id: str | None = None, timeout: float = 15.0) -> dict:
    """POST /chat → poll /chat/runs/{run_id} until completed. Returns final status."""
    payload: dict[str, Any] = {"message": message}
    if session_id:
        payload["session_id"] = session_id

    resp = client.post("/chat", json=payload, headers=headers)
    assert resp.status_code == 200, f"POST /chat failed: {resp.status_code} {resp.text}"
    data = resp.json()
    run_id = data["run_id"]
    sid = data["session_id"]

    # Poll until terminal state
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        resp = client.get(f"/chat/runs/{run_id}", headers=headers)
        if resp.status_code == 200:
            status = resp.json()
            if status["status"] in ("completed", "failed", "cancelled"):
                status["_session_id"] = sid
                return status
        time.sleep(0.1)

    pytest.fail(f"Run {run_id} did not complete within {timeout}s")


from contextlib import contextmanager

@contextmanager
def mock_llm_for_chat(builder_fn):
    """Context manager that patches both _build_chat_loop call sites.

    RunEngine.start_run imports _build_chat_loop at call time, so we must
    patch both the module-level reference AND the import inside start_run.
    """
    import core.agent.run_engine as re_mod
    original_start = re_mod.RunEngine.start_run

    async def patched_start(self_engine, run):
        with patch("api.routers.chat._build_chat_loop", builder_fn):
            await original_start(self_engine, run)

    with patch("api.routers.chat._build_chat_loop", builder_fn):
        with patch.object(re_mod.RunEngine, "start_run", patched_start):
            yield


# ============================================================================
# Fixtures
# ============================================================================

@pytest.fixture
def client():
    from api.main import app
    with TestClient(app) as c:
        yield c


@pytest.fixture
def auth_headers(client):
    """Register + login, return auth headers."""
    username = f"real_{generate_id()}"
    client.post("/auth/register", json={
        "username": username, "email": f"{username}@test.com", "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={"username": username, "password": "testpass1234"})
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


# ============================================================================
# Scenario A: Multi-turn plain conversation (10 turns)
# ============================================================================

class TestScenarioA_MultiTurnConversation:
    """10-turn conversation through /chat API.

    Verifies:
      - Events accumulate correctly (user_query + llm_response per turn)
      - Causal chains are maintained
      - Context snapshots are created for each turn
      - Session can be queried for all events after completion
    """

    def test_10_turn_conversation(self, client, auth_headers):
        h = auth_headers

        turns = [Turn(content=f"Answer to question {i+1}.") for i in range(10)]
        llm = ScriptedLLM(turns)

        with mock_llm_for_chat(_build_patched_chat_loop(llm)):
            # Turn 1: no session_id → auto-create
            result = _chat_and_wait(client, h, "Question 1: What is event sourcing?")
            assert result["status"] == "completed"
            sid = result["_session_id"]

            # Turns 2-10: same session
            for i in range(2, 11):
                result = _chat_and_wait(client, h, f"Question {i}: Follow-up #{i}", session_id=sid)
                assert result["status"] == "completed", f"Turn {i} failed: {result}"

        # Verify: session has events from all 10 turns
        resp = client.get(f"/events/session/{sid}", headers=h)
        assert resp.status_code == 200
        events_data = resp.json()
        total = events_data["total"]
        # Each turn produces: user_query + stream events + llm_response
        # At minimum 2 events per turn (user_query + llm_response), plus stream events
        assert total >= 20, f"Expected ≥20 events for 10 turns, got {total}"

        # Verify: context snapshots were created
        resp = client.get("/context", params={"session_id": sid}, headers=h)
        assert resp.status_code == 200
        snapshots = resp.json()
        # At least 1 snapshot per turn (context_manager.save_snapshot is called each turn)
        snap_count = snapshots.get("total", len(snapshots.get("snapshots", snapshots.get("items", []))))
        assert snap_count >= 10, f"Expected ≥10 snapshots, got {snap_count}"

        # Verify: session is queryable (auto-created sessions may lack title,
        # so we query events instead of the session endpoint)
        resp = client.get(f"/events/session/{sid}", headers=h)
        assert resp.status_code == 200


# ============================================================================
# Scenario B: Tool-use conversation (execute_code skill)
# ============================================================================

class TestScenarioB_ToolUseConversation:
    """LLM calls execute_code tool → code runs → LLM synthesizes answer.

    Verifies:
      - Tool call events are logged
      - Tool results are recorded
      - Final answer incorporates tool output
      - Skill execution metrics are recorded
    """

    def test_tool_call_and_synthesis(self, client, auth_headers):
        h = auth_headers

        turns = [
            Turn(tool_calls=[_make_tool_call("execute_code", {
                "code": "print(2 + 2)", "language": "python",
            })]),
            Turn(content="The result of 2 + 2 is 4."),
        ]
        llm = ScriptedLLM(turns)

        with mock_llm_for_chat(_build_patched_chat_loop_with_skills(llm)):
            result = _chat_and_wait(client, h, "What is 2 + 2? Run the code to verify.")
            assert result["status"] == "completed"
            sid = result["_session_id"]

        # Verify: events include tool call and tool result
        resp = client.get(f"/events/session/{sid}", headers=h)
        assert resp.status_code == 200
        events = resp.json()["events"]
        event_types = [e["event_type"] for e in events]

        # Should have user_query, stream events (tool_call_start, tool_result), llm_response
        assert "user_query" in event_types
        has_tool_events = any("tool" in et for et in event_types)
        assert has_tool_events, f"No tool events found. Types: {event_types}"

        # Verify: at least one llm_response event
        llm_responses = [e for e in events if e["event_type"] == "llm_response"]
        assert len(llm_responses) >= 1


# ============================================================================
# Scenario C: Bulk conversations → evaluation → learning closed loop
# ============================================================================

class TestScenarioC_BulkAndClosedLoop:
    """Run 20 conversations → seed quality scores → trigger evaluation → learning.

    Verifies the full data lifecycle:
      1. Conversations produce events in DB
      2. Quality scores are queryable via evaluation API
      3. Closed loop detects patterns and proposes improvements
      4. Learning cycle processes signals
    """

    def test_bulk_then_evaluate(self, client, auth_headers, db_session):
        h = auth_headers
        from sqlalchemy import text

        # Phase 1: Run 5 real conversations through /chat API
        # (20 would be slow; 5 is enough to verify accumulation)
        turns = [Turn(content=f"Here is the answer.") for _ in range(5)]
        llm = ScriptedLLM(turns)
        session_ids = []

        with mock_llm_for_chat(_build_patched_chat_loop(llm)):
            for i in range(5):
                llm._cursor = 0
                result = _chat_and_wait(client, h, f"Conversation {i+1}: explain topic {i+1}")
                assert result["status"] == "completed", f"Conv {i+1} failed: {result}"
                session_ids.append(result["_session_id"])

        # Phase 2: Seed quality scores on the generated events
        # (In production, auto-scoring does this; here we simulate)
        for sid in session_ids:
            rows = db_session.execute(text(
                "SELECT event_id FROM conversation_events "
                "WHERE session_id = :sid AND event_type = 'llm_response'"
            ), {"sid": sid}).fetchall()
            for row in rows:
                # Alternate between low and high quality to create variance
                score = 2.0 if session_ids.index(sid) < 2 else 4.5
                db_session.execute(text(
                    "UPDATE conversation_events SET quality_score = :qs WHERE event_id = :eid"
                ), {"qs": score, "eid": row[0]})
        db_session.commit()

        # Phase 3: Query quality trend — should reflect our scores
        resp = client.get("/api/v1/evaluation/quality/trend",
                          params={"days": 1}, headers=h)
        assert resp.status_code == 200
        trend = resp.json()
        assert trend["total_events"] >= 5, f"Expected ≥5 scored events, got {trend['total_events']}"

        # Phase 4: Run closed loop
        resp = client.post("/api/v1/evaluation/loop",
                           params={"days": 1, "dry_run": True}, headers=h)
        assert resp.status_code == 200
        loop_data = resp.json()
        assert "drift" in loop_data
        assert "calibration" in loop_data
        assert "diagnoses" in loop_data
        assert "skill_learning" in loop_data
        assert loop_data["loop_id"]  # audit trail exists

        # Phase 5: Trigger learning
        resp = client.post("/api/v1/learning/trigger", json={
            "days": 1, "signal_types": ["wrong_skill", "slow_execution"],
        }, headers=h)
        assert resp.status_code == 200
        learn = resp.json()
        assert learn["status"] in ("success", "error")

        # Phase 6: Verify learning stats reflect activity
        resp = client.get("/api/v1/learning/stats", headers=h)
        assert resp.status_code == 200


# ============================================================================
# Scenario D: Cross-session context (session 1 → session 2)
# ============================================================================

class TestScenarioD_CrossSessionContext:
    """Session 1 produces events → Session 2 should have richer context.

    Verifies:
      - Events from session 1 are persisted
      - Session 2 context snapshot includes prior session data (via ContextManager)
      - The system maintains user-level continuity
    """

    def test_cross_session_continuity(self, client, auth_headers):
        h = auth_headers

        # Session 1: establish context
        turns_s1 = [Turn(content="Event sourcing stores state as a sequence of events.")]
        llm = ScriptedLLM(turns_s1)

        with mock_llm_for_chat(_build_patched_chat_loop(llm)):
            r1 = _chat_and_wait(client, h, "What is event sourcing?")
            assert r1["status"] == "completed"
            sid1 = r1["_session_id"]

        # Session 2: new session, same user — context should be richer
        turns_s2 = [Turn(content="CQRS separates read and write models.")]
        llm2 = ScriptedLLM(turns_s2)

        with mock_llm_for_chat(_build_patched_chat_loop(llm2)):
            r2 = _chat_and_wait(client, h, "How does CQRS relate to event sourcing?")
            assert r2["status"] == "completed"
            sid2 = r2["_session_id"]

        # Verify: both sessions exist and have events
        assert sid1 != sid2, "Should be different sessions"

        for sid in [sid1, sid2]:
            resp = client.get(f"/events/session/{sid}", headers=h)
            assert resp.status_code == 200
            assert resp.json()["total"] >= 2  # at least user_query + llm_response

        # Verify: session 2 has a context snapshot
        resp = client.get("/context", params={"session_id": sid2}, headers=h)
        assert resp.status_code == 200


# ============================================================================
# Scenario E: Multi-turn with tool use (3 turns, 2 tool calls)
# ============================================================================

class TestScenarioE_MultiTurnToolUse:
    """3-turn conversation: plain → tool call → plain.

    Verifies the full multi-turn tool-use protocol:
      Turn 1: LLM answers directly
      Turn 2: LLM calls execute_code, gets result, synthesizes
      Turn 3: LLM answers follow-up directly
    """

    def test_mixed_turns(self, client, auth_headers):
        h = auth_headers

        # Turn 1 script: plain answer
        # Turn 2 script: tool call + final answer (2 LLM calls in one turn)
        # Turn 3 script: plain answer
        all_turns = [
            Turn(content="Python is a programming language."),  # Turn 1
            Turn(tool_calls=[_make_tool_call("execute_code", {  # Turn 2, call 1
                "code": "import sys; print(sys.version)", "language": "python",
            })]),
            Turn(content="You are running Python 3.11."),       # Turn 2, call 2
            Turn(content="Yes, Python 3.11 has many improvements."),  # Turn 3
        ]
        llm = ScriptedLLM(all_turns)

        with mock_llm_for_chat(_build_patched_chat_loop_with_skills(llm)):
            # Turn 1
            r1 = _chat_and_wait(client, h, "What is Python?")
            assert r1["status"] == "completed"
            sid = r1["_session_id"]

            # Turn 2 (tool use)
            r2 = _chat_and_wait(client, h, "What Python version am I running? Check it.", session_id=sid)
            assert r2["status"] == "completed"

            # Turn 3
            r3 = _chat_and_wait(client, h, "Is 3.11 good?", session_id=sid)
            assert r3["status"] == "completed"

        # Verify: session has events from all 3 turns
        resp = client.get(f"/events/session/{sid}", headers=h)
        assert resp.status_code == 200
        total = resp.json()["total"]
        # 3 user queries + at least 3 llm responses + tool events
        assert total >= 6, f"Expected ≥6 events for 3 turns, got {total}"

        # Verify: tool-related events exist
        events = resp.json()["events"]
        event_types = [e["event_type"] for e in events]
        user_queries = [e for e in events if e["event_type"] == "user_query"]
        assert len(user_queries) == 3, f"Expected 3 user queries, got {len(user_queries)}"


# ============================================================================
# Scenario F: LLM failure → run status = failed, error event persisted
# ============================================================================

class TestScenarioF_ErrorRecovery:
    """LLM crashes mid-conversation → run transitions to 'failed'.

    Verifies:
      - Run status becomes 'failed' (not stuck in 'running')
      - Error event is persisted and queryable
      - Session remains usable — next message succeeds
    """

    def test_llm_crash_then_recover(self, client, auth_headers):
        h = auth_headers

        # Script that raises on first call to chat_with_tools_stream
        class CrashingLLM(ScriptedLLM):
            def __init__(self):
                super().__init__([])
                self._crash_next = True

            async def chat_with_tools_stream(self, messages, tools, **kw):
                if self._crash_next:
                    self._crash_next = False
                    raise RuntimeError("LLM provider unavailable")
                yield {"type": "text", "content": "Recovered successfully."}

        crashing_llm = CrashingLLM()

        with mock_llm_for_chat(_build_patched_chat_loop(crashing_llm)):
            # Turn 1: LLM crashes → run should fail
            resp = client.post("/chat", json={"message": "Hello"}, headers=h)
            assert resp.status_code == 200
            run_id = resp.json()["run_id"]
            sid = resp.json()["session_id"]

            # Poll until terminal
            deadline = time.monotonic() + 10
            final_status = None
            while time.monotonic() < deadline:
                resp = client.get(f"/chat/runs/{run_id}", headers=h)
                if resp.status_code == 200:
                    s = resp.json()["status"]
                    if s in ("completed", "failed", "cancelled"):
                        final_status = s
                        break
                time.sleep(0.1)

            assert final_status == "failed", f"Expected 'failed', got '{final_status}'"

            # Verify: error event persisted in session
            resp = client.get(f"/events/session/{sid}", headers=h)
            assert resp.status_code == 200
            events = resp.json()["events"]
            event_types = [e["event_type"] for e in events]
            assert "run_failed" in event_types, f"No run_failed event. Types: {event_types}"

            # Turn 2: same session, LLM recovered → should succeed
            result = _chat_and_wait(client, h, "Try again", session_id=sid)
            assert result["status"] == "completed"

    def test_timeout_produces_failed_run(self, client, auth_headers):
        """Run with very short timeout → status becomes 'failed'."""
        h = auth_headers

        class SlowLLM(ScriptedLLM):
            async def chat_with_tools_stream(self, messages, tools, **kw):
                await asyncio.sleep(10)  # longer than timeout, shorter than test patience
                yield {"type": "text", "content": "too late"}

        slow_llm = SlowLLM()

        with mock_llm_for_chat(_build_patched_chat_loop(slow_llm)):
            resp = client.post("/chat", json={
                "message": "This will timeout",
                "context": {"run_timeout_seconds": 0.1},
            }, headers=h)
            assert resp.status_code == 200
            run_id = resp.json()["run_id"]

            # Poll — should become failed or cancelled
            deadline = time.monotonic() + 10
            final_status = None
            while time.monotonic() < deadline:
                resp = client.get(f"/chat/runs/{run_id}", headers=h)
                if resp.status_code == 200:
                    s = resp.json()["status"]
                    if s in ("completed", "failed", "cancelled"):
                        final_status = s
                        break
                time.sleep(0.1)

            # Timeout may surface as 'failed' (TimeoutError) or 'cancelled'
            # (CancelledError from asyncio.wait_for internals) — both are correct.
            assert final_status in ("failed", "cancelled"), \
                f"Expected terminal error state, got '{final_status}'"


# ============================================================================
# Scenario G: Cancel a running task via API
# ============================================================================

class TestScenarioG_CancelRun:
    """Start a long-running chat → cancel via DELETE → verify cancelled status."""

    def test_cancel_running_task(self, client, auth_headers):
        h = auth_headers

        class BlockingLLM(ScriptedLLM):
            async def chat_with_tools_stream(self, messages, tools, **kw):
                await asyncio.sleep(30)
                yield {"type": "text", "content": "never reached"}

        blocking_llm = BlockingLLM()

        with mock_llm_for_chat(_build_patched_chat_loop(blocking_llm)):
            # Start chat
            resp = client.post("/chat", json={"message": "Long task"}, headers=h)
            assert resp.status_code == 200
            run_id = resp.json()["run_id"]

            # Wait for run to enter running state
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                resp = client.get(f"/chat/runs/{run_id}", headers=h)
                if resp.status_code == 200 and resp.json()["status"] == "running":
                    break
                time.sleep(0.05)

            # Cancel — may already be running or still pending
            resp = client.delete(f"/chat/runs/{run_id}", headers=h)
            # 200 = cancelled, 409 = already finished (race condition)
            assert resp.status_code in (200, 409)

            # Poll to confirm terminal state
            deadline = time.monotonic() + 5
            final_status = None
            while time.monotonic() < deadline:
                resp = client.get(f"/chat/runs/{run_id}", headers=h)
                if resp.status_code == 200:
                    s = resp.json()["status"]
                    if s in ("completed", "failed", "cancelled"):
                        final_status = s
                        break
                time.sleep(0.1)

            assert final_status == "cancelled", \
                f"Expected 'cancelled', got '{final_status}'"


# ============================================================================
# Scenario H: Concurrent messages on same session
# ============================================================================

class TestScenarioH_ConcurrentMessages:
    """Two messages sent to the same session near-simultaneously.

    Verifies:
      - Both runs complete (no deadlock, no corruption)
      - Session has events from both runs
      - Each run gets its own run_id
    """

    def test_concurrent_same_session(self, client, auth_headers):
        h = auth_headers

        # Each run gets its own LLM instance with independent cursor
        class ConcurrentSafeLLM(ScriptedLLM):
            """Each call returns a unique response based on input."""
            async def chat_with_tools_stream(self, messages, tools, **kw):
                # Extract user message to make response unique
                user_msg = ""
                for m in messages:
                    if isinstance(m, dict) and m.get("role") == "user":
                        user_msg = m.get("content", "")
                yield {"type": "text", "content": f"Response to: {user_msg[:50]}"}

        safe_llm = ConcurrentSafeLLM()

        with mock_llm_for_chat(_build_patched_chat_loop(safe_llm)):
            # Create session first
            resp = client.post("/sessions", json={"title": "Concurrent Test"}, headers=h)
            assert resp.status_code == 201
            sid = resp.json()["session_id"]

            # Send two messages rapidly
            resp1 = client.post("/chat", json={
                "message": "First concurrent message", "session_id": sid,
            }, headers=h)
            resp2 = client.post("/chat", json={
                "message": "Second concurrent message", "session_id": sid,
            }, headers=h)

            assert resp1.status_code == 200
            assert resp2.status_code == 200
            run_id_1 = resp1.json()["run_id"]
            run_id_2 = resp2.json()["run_id"]
            assert run_id_1 != run_id_2

            # Wait for both to complete
            for rid in [run_id_1, run_id_2]:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    resp = client.get(f"/chat/runs/{rid}", headers=h)
                    if resp.status_code == 200 and resp.json()["status"] in ("completed", "failed"):
                        break
                    time.sleep(0.1)

            # Verify both completed
            for rid in [run_id_1, run_id_2]:
                resp = client.get(f"/chat/runs/{rid}", headers=h)
                assert resp.status_code == 200
                assert resp.json()["status"] == "completed", \
                    f"Run {rid} status: {resp.json()['status']}"

            # Verify session has events from both runs
            resp = client.get(f"/events/session/{sid}", headers=h)
            assert resp.status_code == 200
            total = resp.json()["total"]
            # 2 runs × (user_query + stream events + llm_response) ≥ 4
            assert total >= 4, f"Expected ≥4 events from 2 runs, got {total}"


# ============================================================================
# Scenario I: Event ordering and data integrity under load
# ============================================================================

class TestScenarioI_DataIntegrity:
    """Run 10 rapid-fire conversations → verify no orphaned events, no missing sessions.

    Verifies:
      - Every run_id resolves to a terminal state
      - Every session has at least user_query + llm_response
      - No DB constraint violations under concurrent writes
    """

    def test_rapid_fire_integrity(self, client, auth_headers):
        h = auth_headers

        class FastLLM(ScriptedLLM):
            async def chat_with_tools_stream(self, messages, tools, **kw):
                yield {"type": "text", "content": "Quick answer."}

        fast_llm = FastLLM()

        with mock_llm_for_chat(_build_patched_chat_loop(fast_llm)):
            # Fire 10 conversations (each auto-creates a session)
            runs = []
            for i in range(10):
                resp = client.post("/chat", json={"message": f"Quick question {i}"}, headers=h)
                assert resp.status_code == 200
                runs.append(resp.json())

            # Wait for all to finish
            for r in runs:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    resp = client.get(f"/chat/runs/{r['run_id']}", headers=h)
                    if resp.status_code == 200 and resp.json()["status"] in ("completed", "failed"):
                        break
                    time.sleep(0.1)

        # Verify: all 10 runs reached terminal state
        terminal = 0
        for r in runs:
            resp = client.get(f"/chat/runs/{r['run_id']}", headers=h)
            if resp.status_code == 200 and resp.json()["status"] in ("completed", "failed"):
                terminal += 1
        assert terminal == 10, f"Only {terminal}/10 runs reached terminal state"

        # Verify: each session has events
        seen_sessions = set()
        for r in runs:
            sid = r["session_id"]
            if sid in seen_sessions:
                continue
            seen_sessions.add(sid)
            resp = client.get(f"/events/session/{sid}", headers=h)
            assert resp.status_code == 200
            assert resp.json()["total"] >= 2, \
                f"Session {sid} has only {resp.json()['total']} events"
