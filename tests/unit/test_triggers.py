"""Tests for trigger system — webhook + cron schedule → AgentRun."""

import json
import pytest
from datetime import datetime, timezone, timedelta
from unittest.mock import MagicMock, patch, AsyncMock

from core.agent.triggers import (
    create_trigger, get_trigger, list_triggers, delete_trigger,
    fire_trigger, advance_schedule, get_due_triggers,
    claim_and_advance, verify_secret, _VALID_TRIGGER_TYPES,
)


@pytest.fixture
def mock_db():
    db = MagicMock()
    db.query.return_value.filter.return_value.first.return_value = None
    db.query.return_value.filter.return_value.all.return_value = []
    db.query.return_value.filter.return_value.order_by.return_value.all.return_value = []
    db.query.return_value.filter.return_value.delete.return_value = 1
    db.query.return_value.filter.return_value.update.return_value = 1
    return db


class TestCreateTrigger:

    def test_create_webhook_trigger(self, mock_db):
        result = create_trigger(
            mock_db, user_id="u1", agent_id="dev-agent",
            trigger_type="webhook", name="deploy-hook",
            user_input="Deploy triggered",
        )
        assert result["trigger_id"]
        assert result["trigger_type"] == "webhook"
        assert result["secret"]
        assert "/fire" in result["webhook_url"]
        mock_db.commit.assert_called()

    def test_create_schedule_trigger(self, mock_db):
        result = create_trigger(
            mock_db, user_id="u1", agent_id="dev-agent",
            trigger_type="schedule", name="daily-report",
            user_input="Generate daily report",
            cron_expr="0 9 * * *",
        )
        assert result["trigger_type"] == "schedule"
        assert result["next_fire_at"]
        assert "secret" not in result

    def test_schedule_requires_cron_expr(self, mock_db):
        with pytest.raises(ValueError, match="cron_expr required"):
            create_trigger(
                mock_db, user_id="u1", agent_id="dev-agent",
                trigger_type="schedule", name="bad",
                user_input="no cron",
            )

    def test_invalid_cron_expr(self, mock_db):
        with pytest.raises(ValueError, match="Invalid cron"):
            create_trigger(
                mock_db, user_id="u1", agent_id="dev-agent",
                trigger_type="schedule", name="bad",
                user_input="bad cron", cron_expr="not a cron",
            )

    def test_invalid_trigger_type(self, mock_db):
        with pytest.raises(ValueError, match="trigger_type must be"):
            create_trigger(
                mock_db, user_id="u1", agent_id="dev-agent",
                trigger_type="banana", name="bad",
                user_input="nope",
            )


class TestFireTrigger:

    def test_fire_webhook(self, mock_db):
        trig = {
            "trigger_id": "t1", "user_id": "u1", "agent_id": "dev-agent",
            "trigger_type": "webhook", "user_input": "deploy",
            "context": None, "is_active": 1, "session_id": None,
            "cron_expr": None, "secret": "s1",
        }

        def factory():
            return MagicMock()

        with patch("core.agent.triggers.get_trigger", return_value=trig), \
             patch("core.agent.run_engine.RunEngine") as MockEngine, \
             patch("core.agent.triggers._auto_session", return_value="sess-1"), \
             patch("asyncio.create_task"):
            mock_run = MagicMock()
            mock_run.run_id = "run-1"
            mock_run.status.value = "pending"
            MockEngine.return_value.create_run.return_value = mock_run

            result = fire_trigger(factory, "t1", payload={"branch": "main"})

        assert result["run_id"] == "run-1"
        assert result["trigger_id"] == "t1"

    def test_fire_inactive_trigger(self, mock_db):
        trig = {"trigger_id": "t1", "is_active": 0, "trigger_type": "webhook"}

        def factory():
            return MagicMock()

        with patch("core.agent.triggers.get_trigger", return_value=trig):
            with pytest.raises(ValueError, match="disabled"):
                fire_trigger(factory, "t1")

    def test_fire_nonexistent_trigger(self, mock_db):
        mock_db.execute.return_value.mappings.return_value.first.return_value = None

        def factory():
            return MagicMock()

        with patch("core.agent.triggers.get_trigger", return_value=None):
            with pytest.raises(ValueError, match="not found"):
                fire_trigger(factory, "nope")


class TestGetDueTriggers:

    def test_returns_due_trigger_ids(self, mock_db):
        mock_db.query.return_value.filter.return_value.all.return_value = [("t1",), ("t2",)]
        result = get_due_triggers(mock_db)
        assert result == ["t1", "t2"]


class TestAdvanceSchedule:

    def test_advances_next_fire(self, mock_db):
        trig = {
            "trigger_id": "t1", "cron_expr": "0 9 * * *",
            "trigger_type": "schedule", "is_active": 1,
        }
        with patch("core.agent.triggers.get_trigger", return_value=trig):
            advance_schedule(mock_db, "t1")
        mock_db.commit.assert_called()


class TestListAndDelete:

    def test_list_triggers(self, mock_db):
        row = MagicMock()
        row.__table__ = MagicMock()
        col = MagicMock()
        col.name = "trigger_id"
        row.__table__.columns = [col]
        row.trigger_id = "t1"
        mock_db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [row]
        result = list_triggers(mock_db, "u1")
        assert len(result) == 1
        assert result[0]["trigger_id"] == "t1"

    def test_delete_trigger(self, mock_db):
        assert delete_trigger(mock_db, "t1") is True
        mock_db.commit.assert_called()


class TestClaimAndAdvance:

    def test_claim_succeeds_when_due(self, mock_db):
        trig = {"trigger_id": "t1", "cron_expr": "0 9 * * *", "is_active": 1}
        with patch("core.agent.triggers.get_trigger", return_value=trig):
            assert claim_and_advance(mock_db, "t1") is True

    def test_claim_fails_when_already_claimed(self, mock_db):
        trig = {"trigger_id": "t1", "cron_expr": "0 9 * * *", "is_active": 1}
        mock_db.query.return_value.filter.return_value.update.return_value = 0
        with patch("core.agent.triggers.get_trigger", return_value=trig):
            assert claim_and_advance(mock_db, "t1") is False

    def test_claim_returns_false_for_missing_trigger(self, mock_db):
        with patch("core.agent.triggers.get_trigger", return_value=None):
            assert claim_and_advance(mock_db, "nope") is False


class TestVerifySecret:

    def test_correct_secret(self):
        assert verify_secret("abc123", "abc123") is True

    def test_wrong_secret(self):
        assert verify_secret("wrong", "abc123") is False

    def test_empty_secret(self):
        assert verify_secret("", "abc123") is False
