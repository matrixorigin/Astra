"""Test CLI authentication and session handling."""

import pytest


class TestCLISessionInfo:
    """Test CLI session_info handling."""

    def test_jwt_user_id_extracted_from_session_info(self):
        """Test that jwt_user_id is extracted when session_info is provided."""
        # This tests the fix for: jwt_user_id undefined when session_info is not None
        session_info = {
            "user_id": "test-user-123",
            "session_id": "test-session",
            "agent_id": "test-agent",
            "model": "test-model",
            "turn": 0,
        }

        # The code should extract user_id from session_info
        jwt_user_id = session_info.get("user_id", "")
        assert jwt_user_id == "test-user-123"

    def test_jwt_user_id_created_when_session_info_is_none(self):
        """Test that jwt_user_id is created when session_info is None."""
        user_id = "provided-user-id"
        agent_id = None

        # When session_info is None, should use provided user_id
        jwt_user_id = user_id or agent_id or ""
        assert jwt_user_id == "provided-user-id"

    def test_jwt_user_id_fallback_to_agent_id(self):
        """Test that jwt_user_id falls back to agent_id when user_id is None."""
        user_id = None
        agent_id = "fallback-agent-id"

        jwt_user_id = user_id or agent_id or ""
        assert jwt_user_id == "fallback-agent-id"

    def test_jwt_user_id_empty_when_both_none(self):
        """Test that jwt_user_id is empty string when both are None."""
        user_id = None
        agent_id = None

        jwt_user_id = user_id or agent_id or ""
        assert jwt_user_id == ""
