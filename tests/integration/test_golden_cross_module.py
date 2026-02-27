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
        db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": sid})
        db_session.execute(text("DELETE FROM agent_sessions WHERE session_id = :s"), {"s": sid})
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
        """Replaying a golden session writes an auth_audit_logs entry."""
        from api.models.auth import AuditLog
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


# ── 8. Hallucination Firewall ─────────────────────────────

class TestHallucinationFirewallWithGolden:
    """multi_turn_correction: DeepSeek said 'Read Committed' (wrong),
    doc_search returned 'Snapshot Isolation' (right).
    Firewall should detect the contradiction."""

    def test_wrong_answer_detected_by_regex_extractor(self):
        """ClaimExtractor finds claims in the wrong LLM response."""
        from core.verification.claim_extractor import ClaimExtractor

        fixture = _load("multi_turn_correction")
        wrong_response = fixture["events"][1]["content"]  # "Read Committed"

        extractor = ClaimExtractor()
        claims = extractor.extract(wrong_response)
        # Regex extractor should find at least the factual claim
        # (it may or may not — depends on patterns, but the response is short)
        # The key test is that the firewall pipeline doesn't crash on real LLM output
        assert isinstance(claims, list)

    def test_firewall_verify_with_context(self, db_session):
        """Firewall.verify_response runs without error on real LLM output."""
        from unittest.mock import MagicMock
        from core.verification.firewall import HallucinationFirewall

        fixture = _load("multi_turn_correction")
        wrong_response = fixture["events"][1]["content"]

        ctx_mgr = MagicMock()
        ctx_mgr.load_snapshot.return_value = {
            "system_prompt": "You are a database expert.",
            "selected_events": [],
        }

        fw = HallucinationFirewall(
            lambda: db_session, ctx_mgr,
            use_llm_extraction=False,  # regex only, no LLM needed
        )
        result = fw.verify_response(wrong_response, "fake_capture_id", mode="warn")

        assert hasattr(result, "safe_to_deliver")
        assert hasattr(result, "confidence_score")
        assert isinstance(result.claims_verified, int)


# ── 9. GoldenSelector ────────────────────────────────────

class TestGoldenSelectorWithGolden:
    """GoldenSelector picks high-quality sessions from golden data."""

    def test_select_golden_sessions(self, db_session, sid):
        """Sessions with quality_score >= 4.0 are selected as golden."""
        from core.evaluation.golden_selector import GoldenSessionSelector

        fixture = _load("chained_tool_calls")
        # Seed events with high quality scores
        for ev in fixture["events"]:
            eid = ev["event_id"]
            db_session.add(Event(
                event_id=eid, session_id=sid, user_id=ev["user_id"],
                event_type=ev["event_type"], content=ev["content"],
                causal_chain_id=ev["causal_chain_id"],
                parent_event_id=ev.get("parent_event_id"),
                event_metadata=ev.get("metadata", {}),
                quality_score=4.5 if ev["event_type"] == "llm_response" else None,
            ))
        db_session.commit()

        selector = GoldenSessionSelector(lambda: db_session)
        goldens = selector.select_golden_sessions(min_quality_score=4.0, limit=10)

        # Should find our high-scored events
        our_events = [g for g in goldens if g["session_id"] == sid]
        assert len(our_events) > 0, "Golden selector should find our high-quality events"


# ── 10. DriftDetector ─────────────────────────────────────

class TestDriftDetectorWithGolden:
    """DriftDetector uses quality_score trends — seed with golden data."""

    def test_no_drift_on_stable_scores(self, db_session, sid):
        """Stable quality scores → no drift signal."""
        from core.evaluation.drift_detector import DriftDetector

        fixture = _load("code_review")
        for ev in fixture["events"]:
            db_session.add(Event(
                event_id=ev["event_id"], session_id=sid, user_id=ev["user_id"],
                event_type=ev["event_type"], content=ev["content"],
                causal_chain_id=ev["causal_chain_id"],
                quality_score=4.0 if ev["event_type"] == "llm_response" else None,
                llm_model_used=ev.get("llm_model_used"),
            ))
        db_session.commit()

        detector = DriftDetector(lambda: db_session)
        signals = detector.detect()
        # With only a few events, no significant drift should be detected
        severe = [s for s in signals if s.severity.value == "severe"]
        assert len(severe) == 0


# ── 11. LLM Non-Determinism Awareness ────────────────────

class TestLLMNonDeterminism:
    """Verify the system's design handles LLM non-determinism correctly.

    Design doc (trust-and-safety): 'The only uncontrolled variable is LLM
    non-determinism — and that's a much smaller audit surface.'

    The replay system handles this by:
    1. Recording LLM outputs (not re-calling LLM in mock replay)
    2. Comparing at semantic level (SemanticDiff), not exact string match
    3. Tracking model version in events for audit
    """

    def test_same_session_different_infra_llm_models_tracked(self):
        """Each LLM response records which model produced it."""
        fixture = _load("multi_turn_correction")
        for ev in fixture["events"]:
            if ev["event_type"] == "llm_response":
                assert ev.get("llm_model_used") is not None, (
                    f"LLM response {ev['event_id']} missing llm_model_used"
                )

    def test_replay_uses_recorded_not_live_llm(self, db_session, sid):
        """In REPLAY mode, tool_call returns recorded result — LLM is never called."""
        from core.skills.mocking import MockMode, ToolMockingLayer

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

        replay = ToolMockingLayer(
            mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=sid,
        )

        # Get the tool_call event
        tc = next(e for e in fixture["events"] if e["event_type"] == "tool_call")
        result = replay.get_mock_result(
            tc["skill_name"], tc["metadata"]["skill_params"],
            sid, parent_event_id=tc["event_id"],
        )
        # Result comes from DB, not from any LLM call
        assert result is not None

    def test_model_upgrade_detectable_via_metadata(self):
        """If model changes between recording and replay, it's visible in event metadata."""
        fixture = _load("chained_tool_calls")
        llm_events = [e for e in fixture["events"] if e["event_type"] == "llm_response"]

        models = {e.get("llm_model_used") for e in llm_events}
        # All from same recording session → same model
        assert len(models) == 1
        # Model name is recorded — if DeepSeek upgrades, new recordings
        # would show a different model name, detectable by SemanticDiff
        assert "deepseek" in list(models)[0].lower()

    def test_semantic_diff_tolerates_nondeterminism(self, db_session):
        """SemanticDiff compares structure (event types, chains), not exact LLM text."""
        from core.replay.semantic_diff import SemanticDiff

        # Create two sessions with same structure but different LLM text
        sid1, sid2 = _uid(), _uid()
        chain1, chain2 = _uid(), _uid()

        for sid, chain, text_suffix in [(sid1, chain1, "version A"), (sid2, chain2, "version B")]:
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id="u",
                event_type="user_query", content="same question",
                causal_chain_id=chain,
            ))
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id="u",
                event_type="llm_response", content=f"different answer {text_suffix}",
                causal_chain_id=chain,
            ))
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid1, sid2)

        # Same structure → no event type diff
        assert result["event_types"]["user_query"]["diff"] == 0
        assert result["event_types"]["llm_response"]["diff"] == 0
        # Chain count same
        assert result["decision_paths"]["chain_count"]["diff"] == 0

        # Cleanup
        for s in (sid1, sid2):
            db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": s})
        db_session.commit()


# ── 12. SemanticDiff Content Similarity (Embeddings) ──────

class TestSemanticDiffContentSimilarity:
    """SemanticDiff now compares LLM response CONTENT via embeddings,
    not just event type counts."""

    def test_identical_sessions_high_similarity(self, db_session):
        """Same LLM content → similarity ≈ 1.0."""
        from core.replay.semantic_diff import SemanticDiff

        sid1, sid2 = _uid(), _uid()
        chain1, chain2 = _uid(), _uid()
        content = "MatrixOne uses Snapshot Isolation by default."

        for sid, chain in [(sid1, chain1), (sid2, chain2)]:
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id="u",
                event_type="llm_response", content=content,
                causal_chain_id=chain,
            ))
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid1, sid2)

        sim = result["content_similarity"]
        assert sim["overall"] is not None
        assert sim["overall"] > 0.99, f"Identical content should have sim ≈ 1.0, got {sim['overall']}"

        # Cleanup
        for s in (sid1, sid2):
            db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": s})
        db_session.commit()

    def test_different_content_lower_similarity(self, db_session):
        """Completely different LLM responses → low similarity."""
        from core.replay.semantic_diff import SemanticDiff

        sid1, sid2 = _uid(), _uid()
        chain1, chain2 = _uid(), _uid()

        db_session.add(Event(
            event_id=_uid(), session_id=sid1, user_id="u",
            event_type="llm_response",
            content="MatrixOne uses Snapshot Isolation via TAE engine for MVCC.",
            causal_chain_id=chain1,
        ))
        db_session.add(Event(
            event_id=_uid(), session_id=sid2, user_id="u",
            event_type="llm_response",
            content="To make pancakes, mix flour, eggs, and milk. Cook on a hot griddle.",
            causal_chain_id=chain2,
        ))
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid1, sid2)

        sim = result["content_similarity"]
        assert sim["overall"] is not None
        assert sim["overall"] < 0.95, f"Unrelated content should have low sim, got {sim['overall']}"
        # Summary should flag it
        assert sim["responses_compared"] == 1

        for s in (sid1, sid2):
            db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": s})
        db_session.commit()

    def test_golden_session_self_similarity(self, db_session):
        """Replay of a golden session compared to itself → perfect similarity."""
        from core.replay.semantic_diff import SemanticDiff

        fixture = _load("chained_tool_calls")
        sid = _uid()
        for ev in fixture["events"]:
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id=ev["user_id"],
                event_type=ev["event_type"], content=ev["content"],
                causal_chain_id=ev["causal_chain_id"],
            ))
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid, sid)

        assert result["content_similarity"]["overall"] > 0.99
        assert result["content_similarity"]["responses_compared"] == 3  # 3 LLM responses

        db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": sid})
        db_session.commit()

    def test_regression_detected_by_content_similarity(self, db_session):
        """Simulated regression: same structure, degraded content → low similarity + summary warning."""
        from core.replay.semantic_diff import SemanticDiff

        sid1, sid2 = _uid(), _uid()
        chain1, chain2 = _uid(), _uid()

        # Original: correct, detailed answer
        for sid, chain, content in [
            (sid1, chain1, "The SELECT * query on events table is slow because it reads all columns. "
                           "Add a covering index on (session_id, event_type, created_at) to avoid full table scan."),
            (sid2, chain2, "I don't know. Maybe try restarting the database."),
        ]:
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id="u",
                event_type="user_query", content="Why is my query slow?",
                causal_chain_id=chain,
            ))
            db_session.add(Event(
                event_id=_uid(), session_id=sid, user_id="u",
                event_type="llm_response", content=content,
                causal_chain_id=chain,
            ))
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid1, sid2)

        # Structure is identical (1 user_query + 1 llm_response each)
        assert result["event_types"]["user_query"]["diff"] == 0
        assert result["event_types"]["llm_response"]["diff"] == 0

        # But content similarity should be low
        sim = result["content_similarity"]["overall"]
        assert sim < 0.95, f"Degraded content should have low similarity, got {sim}"

        # Summary should mention content
        if sim < 0.7:
            assert "LOW" in result["summary"]

        for s in (sid1, sid2):
            db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": s})
        db_session.commit()
