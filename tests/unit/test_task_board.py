"""Tests for agent team task board."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, Mock

import pytest

from core.agents.task_board import Task, TaskBoard


def _mock_db():
    return MagicMock()


class TestTaskBoard:
    def test_create_task_with_event_logger(self):
        db = _mock_db()
        event_logger = Mock()
        event_logger.create_event.return_value = Mock(event_id="evt-123")

        tb = TaskBoard(lambda: db, event_logger=event_logger)
        task_id = tb.create_task(
            team_id="team-1",
            title="Review auth.py",
            description="Check security",
            created_by="lead-agent",
            session_id="sess-1",
        )

        assert task_id == "evt-123"
        event_logger.create_event.assert_called_once()
        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["event_type"] == "team_task"
        assert call_kwargs["content"] == "Review auth.py"
        assert call_kwargs["metadata"]["team_id"] == "team-1"
        assert call_kwargs["metadata"]["status"] == "open"

    def test_create_task_without_event_logger(self):
        db = _mock_db()
        tb = TaskBoard(lambda: db, event_logger=None)
        task_id = tb.create_task(
            team_id="team-1",
            title="Review auth.py",
            description="Check security",
            created_by="lead-agent",
            session_id="sess-1",
        )

        assert task_id is not None
        db.execute.assert_called_once()
        db.commit.assert_called_once()

    def test_claim_task_success(self):
        db = _mock_db()
        db.execute.return_value = Mock(rowcount=1)  # 1 row inserted

        tb = TaskBoard(lambda: db, event_logger=Mock())
        result = tb.claim_task("task-1", "agent-1", "sess-1")

        assert result is True
        db.commit.assert_called_once()
        # Verify atomic INSERT...SELECT WHERE NOT EXISTS was used
        sql_arg = str(db.execute.call_args[0][0])
        assert "WHERE NOT EXISTS" in sql_arg
        params = db.execute.call_args[1] if db.execute.call_args[1] else db.execute.call_args[0][1]
        assert params["parent"] == "task-1"
        assert params["sid"] == "sess-1"

    def test_claim_task_already_claimed(self):
        db = _mock_db()
        db.execute.return_value = Mock(rowcount=0)  # 0 rows = already claimed

        tb = TaskBoard(lambda: db, event_logger=Mock())
        result = tb.claim_task("task-1", "agent-1", "sess-1")

        assert result is False

    def test_claim_task_without_event_logger(self):
        """Without event_logger, same atomic INSERT...SELECT is used."""
        db = _mock_db()
        db.execute.return_value = Mock(rowcount=1)

        tb = TaskBoard(lambda: db, event_logger=None)
        result = tb.claim_task("task-1", "agent-1", "sess-1")

        assert result is True
        db.commit.assert_called_once()
        sql_arg = str(db.execute.call_args[0][0])
        assert "WHERE NOT EXISTS" in sql_arg

    def test_claim_task_without_event_logger_already_claimed(self):
        """INSERT...SELECT inserts 0 rows when already claimed."""
        db = _mock_db()
        db.execute.return_value = Mock(rowcount=0)

        tb = TaskBoard(lambda: db, event_logger=None)
        result = tb.claim_task("task-1", "agent-1", "sess-1")

        assert result is False

    def test_complete_task(self):
        db = _mock_db()
        event_logger = Mock()
        event_logger.create_event.return_value = Mock(event_id="evt-done")

        tb = TaskBoard(lambda: db, event_logger=event_logger)
        tb.complete_task("task-1", "agent-1", "Found 2 issues", "sess-1")

        event_logger.create_event.assert_called_once()
        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["event_type"] == "team_task_done"
        assert call_kwargs["content"] == "Found 2 issues"
        assert call_kwargs["metadata"]["completed_by"] == "agent-1"

    def test_get_open_tasks(self):
        db = _mock_db()
        # ORM query chain: db.query().filter().order_by().all()
        db.query.return_value.filter.return_value.order_by.return_value.all.return_value = [
            (
                "task-1",
                "Review auth.py",
                {
                    "team_id": "team-1",
                    "description": "Check security",
                    "status": "open",
                    "created_by": "lead",
                },
                datetime(2026, 1, 1, tzinfo=timezone.utc),
            ),
        ]

        tb = TaskBoard(lambda: db)
        tasks = tb.get_open_tasks("team-1", "sess-1")

        assert len(tasks) == 1
        assert tasks[0].task_id == "task-1"
        assert tasks[0].title == "Review auth.py"
        assert tasks[0].status == "open"
        assert tasks[0].created_by == "lead"

    def test_send_message(self):
        db = _mock_db()
        event_logger = Mock()
        event_logger.create_event.return_value = Mock(event_id="msg-1")

        tb = TaskBoard(lambda: db, event_logger=event_logger)
        msg_id = tb.send_message(
            to_agent="agent-2",
            content="Please review my fix",
            from_agent="agent-1",
            session_id="sess-1",
        )

        assert msg_id == "msg-1"
        event_logger.create_event.assert_called_once()
        call_kwargs = event_logger.create_event.call_args[1]
        assert call_kwargs["event_type"] == "agent_message"
        assert call_kwargs["metadata"]["to_agent"] == "agent-2"
        assert call_kwargs["metadata"]["from_agent"] == "agent-1"

    def test_get_messages_for_agent(self):
        db = _mock_db()
        # ORM query chain: db.query().filter().order_by().limit().all()
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = [
            (
                "msg-1",
                "Please review my fix",
                {"to_agent": "agent-2", "from_agent": "agent-1"},
                datetime(2026, 1, 1, tzinfo=timezone.utc),
            ),
        ]

        tb = TaskBoard(lambda: db)
        messages = tb.get_messages_for_agent("agent-2", "sess-1")

        assert len(messages) == 1
        assert messages[0]["message_id"] == "msg-1"
        assert messages[0]["from_agent"] == "agent-1"
        assert messages[0]["content"] == "Please review my fix"

    def test_get_messages_for_agent_empty(self):
        db = _mock_db()
        db.query.return_value.filter.return_value.order_by.return_value.limit.return_value.all.return_value = []

        tb = TaskBoard(lambda: db)
        messages = tb.get_messages_for_agent("agent-1", "sess-1")

        assert messages == []


class TestTask:
    def test_task_creation(self):
        task = Task(
            task_id="t1",
            team_id="team-1",
            title="Review code",
            description="Check for bugs",
            status="open",
            assigned_to=None,
            created_by="lead",
            created_at=datetime(2026, 1, 1, tzinfo=timezone.utc),
        )

        assert task.task_id == "t1"
        assert task.status == "open"
        assert task.assigned_to is None

    def test_task_with_completion(self):
        task = Task(
            task_id="t1",
            team_id="team-1",
            title="Review code",
            description="Check for bugs",
            status="done",
            assigned_to="agent-1",
            created_by="lead",
            created_at=datetime(2026, 1, 1, tzinfo=timezone.utc),
            completed_at=datetime(2026, 1, 2, tzinfo=timezone.utc),
            result="Found 3 issues",
        )

        assert task.status == "done"
        assert task.result == "Found 3 issues"
