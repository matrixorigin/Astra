"""Unit tests for MemoryProgramTool session_info wiring.

Regression tests for the bug where interactive mode's state['session_info']
was missing 'user_id', causing memory writes to fall back to agent_id
('default-agent') instead of the authenticated user's UUID.
"""

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

from cli.tools.memory_program import MemoryProgramTool


class TestSessionInfoUserIdWiring:
    def test_session_info_user_id_used_over_agent_id(self):
        """user_id from session_info must take priority over agent_id."""
        tool = MemoryProgramTool(
            session_info={
                "session_id": "sess-1",
                "agent_id": "default-agent",
                "user_id": "uuid-123",
                "model": None,
                "turn": 0,
            }
        )
        # Simulate execute() user_id resolution
        session = tool._session
        resolved = session.get("user_id") or session.get("agent_id", "")
        assert resolved == "uuid-123"
        assert resolved != "default-agent"

    def test_missing_user_id_falls_back_to_agent_id(self):
        """Without user_id in session_info, falls back to agent_id (old bug behavior)."""
        tool = MemoryProgramTool(
            session_info={
                "session_id": "sess-1",
                "agent_id": "default-agent",
                # no user_id — this was the bug
            }
        )
        session = tool._session
        resolved = session.get("user_id") or session.get("agent_id", "")
        assert resolved == "default-agent"  # documents the fallback

    def test_empty_user_id_falls_back_to_agent_id(self):
        """Empty string user_id (falsy) falls back to agent_id."""
        tool = MemoryProgramTool(
            session_info={
                "session_id": "sess-1",
                "agent_id": "default-agent",
                "user_id": "",  # empty string is falsy
            }
        )
        session = tool._session
        resolved = session.get("user_id") or session.get("agent_id", "")
        assert resolved == "default-agent"

    def test_execute_uses_session_user_id(self):
        """execute() writes memory with the JWT UUID, not agent_id."""
        tool = MemoryProgramTool(
            session_info={
                "session_id": "sess-1",
                "agent_id": "default-agent",
                "user_id": "jwt-uuid-456",
            }
        )

        captured_user_id = None

        mock_result = MagicMock()
        mock_result.actions_executed = 1
        mock_result.actions_failed = 0
        mock_result.experiment_id = None
        mock_result.dry_run = False
        mock_result.rolled_back = False
        mock_result.timed_out = False
        mock_result.results = []

        mock_programmer = MagicMock()
        mock_programmer.execute.side_effect = lambda uid, *a, **kw: (
            captured_user_id.__setitem__(0, uid) or mock_result  # capture uid
        )

        # Use a list to capture since nonlocal doesn't work in nested lambda
        captured = []

        def fake_execute(uid, *a, **kw):
            captured.append(uid)
            return mock_result

        mock_programmer.execute.side_effect = fake_execute

        with patch("cli.tools.memory_program._get_programmer", return_value=mock_programmer):
            asyncio.run(
                tool.execute(
                    actions=[{"inject": {"content": "test", "type": "profile"}}],
                )
            )

        assert captured == ["jwt-uuid-456"]
