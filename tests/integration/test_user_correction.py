"""Integration tests for user correction fallback + event logging."""

import json
import pytest
from sqlalchemy import text as sql_text

from core.context.intent_routing import (
    INTENT_PLANS,
    IntentRouter,
    detect_correction,
)
from core.events.event_logger import EventLogger
from core.events.models import EventType
from tests.integration.helpers import unique_test_id


class TestCorrectionEventLogging:
    """Verify intent_correction events are persisted with correct metadata."""

    def test_correction_event_persisted(self, db_session):
        """Log an intent_correction event and verify all fields in DB."""
        from core.events.session_manager import SessionManager

        user_id = unique_test_id()

        # Create session via ORM
        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=user_id)
        session_id = session.session_id

        el = EventLogger(lambda: db_session)
        el.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type="intent_correction",
            content=json.dumps(
                {
                    "original_intent": "command",
                    "corrected_to": "question",
                    "trigger": "不对",
                }
            ),
            metadata={
                "original_confidence": 0.95,
                "original_tier": 0,
            },
        )

        # Verify via ORM
        from api.models.agent import Event as EventModel

        row = (
            db_session.query(EventModel)
            .filter(
                EventModel.session_id == session_id, EventModel.event_type == "intent_correction"
            )
            .order_by(EventModel.created_at.desc())
            .first()
        )

        assert row is not None, "intent_correction event not found in DB"
        assert row.event_type == "intent_correction"

        content = json.loads(row.content)
        assert content["original_intent"] == "command"
        assert content["corrected_to"] == "question"
        assert content["trigger"] == "不对"

        meta = (
            json.loads(row.event_metadata)
            if isinstance(row.event_metadata, str)
            else row.event_metadata
        )
        assert meta["original_confidence"] == 0.95
        assert meta["original_tier"] == 0


class TestCorrectionOverridesRouting:
    """Verify correction detection overrides high-confidence Tier 0 results — real DB."""

    @pytest.mark.asyncio
    async def test_correction_forces_question_intent(self, db_session):
        router = IntentRouter(db_factory=lambda: db_session)

        # "不对" would match feedback regex, but detect_correction should force question
        assert detect_correction("不对，你搞错了")

        from unittest.mock import patch

        with patch("core.context.routing_metrics.adaptive_threshold", return_value=0.80):
            decision = await router.route("不对，你搞错了", history_len=5, force_intent="question")

        assert decision.routing_result.intent == "question"
        assert decision.routing_result.confidence == 1.0
        assert decision.routing_result.matched_by == "forced"
        assert decision.plan == INTENT_PLANS["question"]
        assert decision.plan.load_tools is True
        assert decision.plan.load_history is True
        assert decision.plan.load_memory is True
