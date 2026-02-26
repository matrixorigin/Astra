"""Golden session cross-module verification tests.

Uses real DeepSeek conversation fixtures to verify capabilities beyond replay:
EventLogger, EventReader, AuditLogger, StreamReplay, token cost, quality scoring.
"""

import json
from datetime import datetime, timezone
from pathlib import Path

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models import Event, Session as SessionModel
from core.events.event_logger import EventLogger
from core.events.models import ConversationEvent, EventType, TokenUsage

FIXTURE_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "golden_sessions"

def _load(name: str) -> dict:
    return json.loads((FIXTURE_DIR / f"{name}.json").read_text())

def _uid():
    return str(uuid7())


@pytest.fixture
def sid():
    return _uid()

@pytest.fixture(autouse=True)
def cleanup(db_session, sid):
    yield
    try:
        db_session.execute(text("DELETE FROM conversation_events WHERE session_id = :s"), {"s": sid})
        db_session.execute(text("DELETE FROM sessions WHERE session_id = :s"), {"s": sid})
        db_session.commit()
    except Exception:
        db_session.rollback()


# ── 1. EventLogger ────────────────────────────────────────

class TestEventLoggerWithGolden:
    """EventLogger correctly persists golden session events."""

    def test_log_event_persists_all_fields(self, db_session, sid):
        """log_event writes content, metadata, token_usage, causal chain."""
        fixture = _load("multi_turn_correction")
        logger = EventLogger.from_session(db_session)

        ev_data = fixture["events"][1]  # llm_response with token_usage
        event = ConversationEvent(
            event_id=_uid(),
            user_id=ev_data["user_id"],
            session_id=sid,
            agent_id="dev-agent",
            agent_version="1.0.0",
            event_type=EventType.LLM_RESPONSE,
            content=ev_data["content"],
            metadata=ev_data.get("metadata"),
            token_usage=TokenUsage(**ev_data["token_usage"]) if ev_data.get("token_usage") else None,
            causal_chain_id=ev_data["causal_chain_id"],
            parent_event_id=ev_data.get("parent_event_id"),
            llm_model_used=ev_data.get("llm_model_used"),
        )
        logger.log_event(event)

        row = db_session.query(Event).filter(Event.event_id == event.event_id).one()
        assert row.content == ev_data["content"]
        assert row.llm_model_used == ev_data.get("llm_model_used")
        assert row.token_usage is not None
        assert row.token_usage["total"] == ev_data["token_usage"]["total"]

    def test_log_full_golden_session(self, db_session, sid):
        """All events from a golden session can be logged and retrieved."""
        fixture = _load("chained_tool_calls")
        logger = EventLogger.from_session(db_session)

        for ev in fixture["events"]:
            event = ConversationEvent(
                event_id=ev["event_id"],
                user_id=ev["user_id"],
                session_id=sid,
                agent_id="dev-agent",
                agent_version="1.0.0",
                event_type=ev["event_type"],
                content=ev["content"],
                metadata=ev.get("metadata"),
                token_usage=TokenUsage(**ev["token_usage"]) if ev.get("token_usage") else None,
                causal_chain_id=ev["causal_chain_id"],
                parent_event_id=ev.get("parent_event_id"),
                llm_model_used=ev.get("llm_model_used"),
                skill_name=ev.get("skill_name"),
                skill_version=ev.get("skill_version"),
            )
            # skill_name/skill_version not on ConversationEvent — set via metadata
            logger.log_event(event)

        count = db_session.query(Event).filter(Event.session_id == sid).count()
        assert count == fixture["event_count"]


# ── 2. EventReader ────────────────────────────────────────

class TestEventReaderWithGolden:
    """EventReader queries golden session events correctly."""

    def _seed(self, db, sid, fixture):
        for ev in fixture["events"]:
            db.add(Event(
                event_id=ev["event_id"], session_id=sid, user_id=ev["user_id"],
                event_type=ev["event_type"], content=ev["content"],
                causal_chain_id=ev["causal_chain_id"],
                parent_event_id=ev.get("parent_event_id"),
                skill_name=ev.get("skill_name"), skill_version=ev.get("skill_version"),
                event_metadata=ev.get("metadata", {}),
                token_usage=ev.get("token_usage"),
                llm_model_used=ev.get("llm_model_used"),
                created_at=datetime.now(timezone.utc),
            ))
        db.commit()

    def test_get_session_events(self, db_session, sid):
        """get_session_events returns all events for a golden session."""
        from core.events.event_reader import EventReader

        fixture = _load("chained_tool_calls")
        self._seed(db_session, sid, fixture)

        reader = EventReader(lambda: db_session)
        events = reader.get_session_events(sid)

        assert len(events) == fixture["event_count"]
        types = [e.event_type for e in events]
        assert "user_query" in types
        assert "tool_call" in types
        assert "tool_result" in types
        assert "llm_response" in types

    def test_get_causal_chain(self, db_session, sid):
        """get_causal_chain returns all events in chronological order."""
        from core.events.event_reader import EventReader

        fixture = _load("multi_turn_correction")
        self._seed(db_session, sid, fixture)

        chain_id = fixture["events"][0]["causal_chain_id"]
        reader = EventReader(lambda: db_session)
        chain = reader.get_causal_chain(chain_id)

        assert len(chain) == fixture["event_count"]
        # Chronological order: created_at ascending
        for i in range(1, len(chain)):
            assert chain[i].created_at >= chain[i - 1].created_at


# ── 3. AuditLogger ────────────────────────────────────────

class TestAuditLoggerWithGolden:
    """ReplayService replay generates audit log entries."""

    def test_replay_creates_audit_log(self, db_session, sid):
        """Replaying a golden session writes an audit_logs entry."""
        from api.models.infra import AuditLog
        from api.services.replay_service import ReplayService

        fixture = _load("code_review")
        uid = fixture["user_id"]

        db_session.add(SessionModel(session_id=sid, user_id=uid, status="active"))
        for ev in fixture["events"]:
            db_session.add(Event(
                event_id=ev["event_id"], session_id=sid, user_id=uid,
                event_type=ev["event_type"], content=ev["content"],
                causal_chain_id=ev["causal_chain_id"],
                parent_event_id=ev.get("parent_event_id"),
                skill_name=ev.get("skill_name"), skill_version=ev.get("skill_version"),
                event_metadata=ev.get("metadata", {}),
            ))
        db_session.commit()

        # Count audit logs before
        before = db_session.query(AuditLog).filter(
            AuditLog.action == "session_replay",
            AuditLog.resource_id == sid,
        ).count()

        svc = ReplayService(lambda: db_session)
        svc.replay_session(session_id=sid, user_id=uid, mock_mode=True)

        after = db_session.query(AuditLog).filter(
            AuditLog.action == "session_replay",
            AuditLog.resource_id == sid,
        ).count()

        assert after == before + 1

        log = db_session.query(AuditLog).filter(
            AuditLog.action == "session_replay",
            AuditLog.resource_id == sid,
        ).order_by(AuditLog.created_at.desc()).first()
        assert log.user_id == uid
        assert log.details["events_count"] == fixture["event_count"]


# ── 4. StreamReplay ───────────────────────────────────────

class TestStreamReplayWithGolden:
    """StreamReplay reconstructs stream from golden session events."""

    def _seed_stream_events(self, db, sid, fixture):
        """Seed golden events as stream_* event types for StreamReplay."""
        chain_id = fixture["events"][0]["causal_chain_id"]
        uid = fixture["user_id"]

        for ev in fixture["events"]:
            if ev["event_type"] == "llm_response":
                content_json = json.dumps({
                    "event_type": "text_message_content",
                    "data": {"delta": ev["content"]},
                })
                db.add(Event(
                    event_id=ev["event_id"], session_id=sid, user_id=uid,
                    event_type="stream_text_done",
                    content=content_json,
                    causal_chain_id=chain_id,
                    parent_event_id=ev.get("parent_event_id"),
                    event_metadata={"event_type": "text_message_content", "agent_id": "dev-agent"},
                    created_at=datetime.now(timezone.utc),
                ))
            elif ev["event_type"] == "tool_call":
                content_json = json.dumps({
                    "event_type": "tool_call_start",
                    "data": {"tool_name": ev.get("skill_name", "unknown")},
                })
                db.add(Event(
                    event_id=ev["event_id"], session_id=sid, user_id=uid,
                    event_type="stream_tool_call_start",
                    content=content_json,
                    causal_chain_id=chain_id,
                    parent_event_id=ev.get("parent_event_id"),
                    event_metadata={"event_type": "tool_call_start", "agent_id": "dev-agent"},
                    created_at=datetime.now(timezone.utc),
                ))
        db.commit()
        return chain_id

    @pytest.mark.asyncio
    async def test_replay_stream_yields_events(self, db_session, sid):
        """StreamReplay.replay_stream yields events from golden session."""
        from core.agent.stream_replay import StreamReplay

        fixture = _load("code_review")
        chain_id = self._seed_stream_events(db_session, sid, fixture)

        sr = StreamReplay(lambda: db_session)
        events = []
        async for ev in sr.replay_stream(sid, causal_chain_id=chain_id):
            events.append(ev)

        assert len(events) > 0
        types = {e.event_type.value if hasattr(e.event_type, 'value') else e.event_type for e in events}
        # Should have text and tool events
        assert types & {"text_message_content", "tool_call_start"}

    @pytest.mark.asyncio
    async def test_get_stream_state_at(self, db_session, sid):
        """get_stream_state_at returns accumulated state up to timestamp."""
        from core.agent.stream_replay import StreamReplay

        fixture = _load("chained_tool_calls")
        chain_id = self._seed_stream_events(db_session, sid, fixture)

        sr = StreamReplay(lambda: db_session)
        state = sr.get_stream_state_at(
            sid, datetime.now(timezone.utc), causal_chain_id=chain_id,
        )

        assert state["session_id"] == sid
        assert len(state["events"]) > 0


# ── 5. Token Cost Statistics ──────────────────────────────

class TestTokenCostWithGolden:
    """Verify token usage statistics from golden sessions."""

    def test_total_tokens_from_golden_session(self):
        """Sum token usage across all LLM events in a golden session."""
        fixture = _load("chained_tool_calls")

        total_prompt = 0
        total_completion = 0
        llm_events = 0
        for ev in fixture["events"]:
            if ev.get("token_usage"):
                total_prompt += ev["token_usage"]["prompt"]
                total_completion += ev["token_usage"]["completion"]
                llm_events += 1

        assert llm_events == 3, "chained_tool_calls has 3 LLM responses"
        assert total_prompt > 0
        assert total_completion > 0
        assert total_prompt + total_completion == sum(
            ev["token_usage"]["total"] for ev in fixture["events"] if ev.get("token_usage")
        )

    def test_cost_calculation_with_real_tokens(self):
        """ModelRouter.calculate_cost produces non-zero cost for real token counts."""
        from core.llm.router import ModelRouter, ModelConfig, ModelPricing
        from core.llm.models import LLMProvider

        router = ModelRouter()
        router.register(ModelConfig(
            model_name="deepseek-chat",
            provider=LLMProvider.DEEPSEEK,
            pricing=ModelPricing(prompt=0.00014, completion=0.00028),  # per 1K tokens (DeepSeek V3)
        ))

        fixture = _load("multi_turn_correction")
        total_cost = 0.0
        for ev in fixture["events"]:
            if ev.get("token_usage"):
                cost = router.calculate_cost(
                    "deepseek-chat",
                    ev["token_usage"]["prompt"],
                    ev["token_usage"]["completion"],
                )
                assert cost > 0, f"Cost should be > 0 for {ev['token_usage']}"
                total_cost += cost

        assert total_cost > 0
        # DeepSeek is cheap — multi_turn_correction should be < $0.01
        assert total_cost < 0.01, f"Unexpectedly high cost: ${total_cost}"

    def test_token_usage_persisted_and_readable(self, db_session, sid):
        """Token usage written via EventLogger is readable via EventReader."""
        from core.events.event_reader import EventReader

        fixture = _load("code_review")
        logger = EventLogger.from_session(db_session)

        llm_ev = next(e for e in fixture["events"] if e.get("token_usage"))
        event = ConversationEvent(
            event_id=_uid(), user_id=llm_ev["user_id"], session_id=sid,
            agent_id="dev-agent", agent_version="1.0.0",
            event_type=EventType.LLM_RESPONSE, content=llm_ev["content"],
            token_usage=TokenUsage(**llm_ev["token_usage"]),
            causal_chain_id=llm_ev["causal_chain_id"],
        )
        logger.log_event(event)

        reader = EventReader(lambda: db_session)
        read_event = reader.get_event(event.event_id)
        assert read_event is not None
        assert read_event.token_usage is not None
        assert read_event.token_usage.total == llm_ev["token_usage"]["total"]


# ── 6. Context Snapshot Reconstruction ────────────────────

class TestContextSnapshotWithGolden:
    """Reconstruct what the LLM saw at each turn from golden session events."""

    def test_reconstruct_llm_context_at_each_turn(self):
        """For each LLM response, reconstruct the message history it saw."""
        fixture = _load("multi_turn_correction")
        events = fixture["events"]

        # Walk events, build message history as the LLM would see it
        messages = []
        llm_turn = 0
        for ev in events:
            if ev["event_type"] == "user_query":
                messages.append({"role": "user", "content": ev["content"]})
            elif ev["event_type"] == "llm_response":
                llm_turn += 1
                # At this point, `messages` is what the LLM saw
                assert len(messages) >= 1, f"LLM turn {llm_turn} had no context"
                assert messages[-1]["role"] == "user", f"LLM turn {llm_turn}: last message should be user"
                messages.append({"role": "assistant", "content": ev["content"]})
            elif ev["event_type"] == "tool_result":
                # Tool results get injected as context for next LLM call
                messages.append({"role": "user", "content": f"Tool result: {ev['content']}"})

        # multi_turn_correction has 3 LLM responses
        assert llm_turn == 3
        # Final message history should have all turns
        assert len(messages) == 7  # 3 user + 3 assistant + 1 tool_result-as-user

    def test_context_grows_monotonically(self):
        """Each successive LLM call sees strictly more context than the previous."""
        fixture = _load("chained_tool_calls")
        events = fixture["events"]

        context_sizes = []
        messages = []
        for ev in events:
            if ev["event_type"] in ("user_query", "tool_result"):
                messages.append(ev["content"])
            elif ev["event_type"] == "llm_response":
                context_sizes.append(len(messages))
                messages.append(ev["content"])

        # Each LLM call should see more context
        for i in range(1, len(context_sizes)):
            assert context_sizes[i] > context_sizes[i - 1], (
                f"Context did not grow: turn {i} saw {context_sizes[i]} msgs, "
                f"turn {i-1} saw {context_sizes[i-1]}"
            )


# ── 7. Quality Scoring ────────────────────────────────────

class TestQualityScoringWithGolden:
    """Auto-scorer produces consistent scores for golden session LLM responses."""

    def test_auto_score_golden_responses(self):
        """compute_auto_score produces valid scores for real LLM responses."""
        from core.evaluation.auto_scorer import compute_auto_score

        fixture = _load("chained_tool_calls")
        scores = []
        for ev in fixture["events"]:
            if ev.get("token_usage") and ev["event_type"] == "llm_response":
                result = compute_auto_score(
                    firewall_passed=True,
                    firewall_confidence=0.9,
                    response_tokens=ev["token_usage"]["completion"],
                )
                assert 0 <= result.quality_score <= 5
                scores.append(result.quality_score)

        assert len(scores) == 3
        # With firewall_passed=True and confidence=0.9, scores should be decent
        assert all(s >= 2.0 for s in scores)

    def test_update_quality_score_persists(self, db_session, sid):
        """EventLogger.update_quality_score writes to DB, readable back."""
        from core.evaluation.auto_scorer import compute_auto_score

        fixture = _load("code_review")
        logger = EventLogger.from_session(db_session)

        llm_ev = next(e for e in fixture["events"] if e.get("token_usage"))
        eid = _uid()
        event = ConversationEvent(
            event_id=eid, user_id=llm_ev["user_id"], session_id=sid,
            agent_id="dev-agent", agent_version="1.0.0",
            event_type=EventType.LLM_RESPONSE, content=llm_ev["content"],
            token_usage=TokenUsage(**llm_ev["token_usage"]),
            causal_chain_id=llm_ev["causal_chain_id"],
        )
        logger.log_event(event)

        result = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.85,
            response_tokens=llm_ev["token_usage"]["completion"],
        )
        logger.update_quality_score(eid, result.quality_score, result.training_eligible)

        row = db_session.query(Event).filter(Event.event_id == eid).one()
        assert row.quality_score == result.quality_score
        assert bool(row.training_eligible) == result.training_eligible
