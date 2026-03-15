"""Unit tests for CLI command parsing and argument handling.

⚠️ IMPORTANT: These are NOT end-to-end tests!

These tests use mocks and only verify:
- Click command structure and argument parsing
- CLI calls the correct API Client methods with correct parameters
- Error handling and output formatting

They do NOT test:
- Real HTTP requests to API server
- API response format correctness
- Database persistence
- Complete CLI → API → DB flow

For true end-to-end tests, see tests/integration/test_real_e2e.py

Note: Mock return values should match real API response formats.
If API changes response format, these tests must be updated.
"""

import json
from unittest.mock import MagicMock, patch

import pytest
from click.testing import CliRunner

from cli.mo_admin_api import cli as admin_cli
from cli.mo_agent_api import cli as agent_cli


@pytest.fixture
def runner():
    return CliRunner()


@pytest.fixture
def mock_api_client():
    """Mock API client to verify CLI → API calls.

    Note: SyncAPIClient uses __getattr__ to proxy methods to APIClient,
    so spec=SyncAPIClient would block access to proxied methods like
    login(), list_skills(), etc.  Instead we use unspecced MagicMock
    and rely on assert_called_once_with() to catch parameter errors.
    """
    with patch("cli.mo_agent_api.SyncAPIClient") as mock:
        client_instance = MagicMock()
        # _run must actually execute coroutines for chat tests.
        # Use a dedicated event loop (mirrors real SyncAPIClient behaviour).
        import asyncio

        _loop = asyncio.new_event_loop()
        client_instance._run.side_effect = _loop.run_until_complete
        client_instance._ensure_client.return_value = client_instance
        mock.return_value = client_instance
        yield client_instance
        _loop.close()


@pytest.fixture
def mock_admin_api_client():
    """Mock API client for admin CLI."""
    with patch("cli.mo_admin_api.SyncAPIClient") as mock:
        client_instance = MagicMock()
        mock.return_value = client_instance
        yield client_instance


class TestAgentCLIToAPI:
    """Test CLI commands correctly call API client methods."""

    def test_login_calls_api_client(self, runner, mock_api_client):
        """Test login command calls API client login method."""
        mock_api_client.login.return_value = {"email": "test@example.com"}

        result = runner.invoke(agent_cli, ["login"], input="test@example.com\npassword123\n")

        assert result.exit_code == 0
        mock_api_client.login.assert_called_once_with("test@example.com", "password123")
        assert "✅ Logged in" in result.output

    def test_register_calls_api_client(self, runner, mock_api_client):
        """Test register command calls API client register method."""
        mock_api_client.register.return_value = {"email": "new@example.com"}

        result = runner.invoke(
            agent_cli, ["register"], input="new@example.com\npassword123\npassword123\ntestuser\n"
        )

        assert result.exit_code == 0
        mock_api_client.register.assert_called_once_with(
            "testuser", "password123", "new@example.com"
        )
        assert "✅ Registered" in result.output

    def test_session_list_calls_api_client(self, runner, mock_api_client):
        """Test session list command calls API client."""
        mock_api_client.ensure_authenticated.return_value = True
        # API returns {"sessions": [...], "total": ...}
        mock_api_client.list_sessions.return_value = {
            "sessions": [
                {"session_id": "sess-1", "user_id": "alice", "status": "active", "event_count": 5},
                {"session_id": "sess-2", "user_id": "alice", "status": "closed", "event_count": 3},
            ],
            "total": 2,
        }

        # API filters by JWT automatically, no --user-id parameter
        result = runner.invoke(agent_cli, ["session", "list"])

        assert result.exit_code == 0
        mock_api_client.list_sessions.assert_called_once_with(agent_id=None, status=None, limit=20)
        assert "sess-1" in result.output
        assert "sess-2" in result.output

    def test_session_show_calls_api_client(self, runner, mock_api_client):
        """Test session show command calls API client."""
        mock_api_client.get_session.return_value = {
            "session_id": "sess-123",
            "user_id": "alice",
            "status": "active",
            "event_count": 5,
        }

        result = runner.invoke(agent_cli, ["session", "show", "sess-123"])

        assert result.exit_code == 0
        mock_api_client.get_session.assert_called_once_with("sess-123")
        assert "sess-123" in result.output
        assert "alice" in result.output

    def test_skill_list_calls_api_client(self, runner, mock_api_client):
        """Test skill list command calls API client."""
        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.list_skills.return_value = [
            {
                "skill_name": "code_search",
                "version": "1.0",
                "is_active": True,
                "description": "Search code",
            },
            {
                "skill_name": "web_search",
                "version": "1.0",
                "is_active": True,
                "description": "Search web",
            },
        ]

        result = runner.invoke(agent_cli, ["skill", "list"])

        assert result.exit_code == 0
        mock_api_client.list_skills.assert_called_once_with()
        assert "code_search" in result.output

    def test_skill_register_calls_api_client(self, runner, mock_api_client, tmp_path):
        """Test skill register command calls API client."""
        skill_file = tmp_path / "skill.json"
        skill_data = {"skill_name": "test_skill", "version": "1.0", "description": "Test skill"}
        skill_file.write_text(json.dumps(skill_data))

        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.register_skill.return_value = {"skill_name": "test_skill", "version": "1.0"}

        result = runner.invoke(agent_cli, ["skill", "register", str(skill_file)])

        assert result.exit_code == 0
        mock_api_client.register_skill.assert_called_once()
        assert "✅ Registered" in result.output

    def test_replay_calls_api_client(self, runner, mock_api_client):
        """Test replay command calls API client."""
        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.replay_session.return_value = {
            "replay_id": "replay-123",
            "events_replayed": 5,
        }

        result = runner.invoke(agent_cli, ["replay", "sess-123"])

        assert result.exit_code == 0
        # Check that replay_session was called (don't enforce exact kwargs)
        mock_api_client.replay_session.assert_called_once()
        assert "Replayed 5 events" in result.output


class TestAdminCLIToAPI:
    """Test admin CLI commands correctly call API client methods."""

    def test_admin_register_calls_api_client(self, runner, mock_admin_api_client):
        """Test admin register command calls admin_register method."""
        mock_admin_api_client.admin_register.return_value = {"username": "newadmin"}

        result = runner.invoke(admin_cli, ["register"], input="newadmin\npassword123\n")

        assert result.exit_code == 0
        mock_admin_api_client.admin_register.assert_called_once_with(
            username="newadmin", password="password123", email="newadmin@example.com"
        )
        assert "✅ Admin registered" in result.output

    def test_token_create_calls_api_client(self, runner, mock_admin_api_client):
        """Test token create command calls API client."""
        mock_admin_api_client.ensure_authenticated.return_value = True
        mock_admin_api_client.admin_create_token.return_value = {
            "token_id": "tok-123",
            "token_type": "llm",
            "provider": "openai",
        }

        result = runner.invoke(
            admin_cli,
            ["token", "create", "--type", "llm", "--provider", "openai"],
            input="sk-test-key\n",
        )

        assert result.exit_code == 0
        mock_admin_api_client.admin_create_token.assert_called_once()
        assert "✅" in result.output or "tok-123" in result.output

    def test_token_list_calls_api_client(self, runner, mock_admin_api_client):
        """Test token list command calls API client."""
        mock_admin_api_client.ensure_authenticated.return_value = True
        mock_admin_api_client.admin_list_tokens.return_value = [
            {
                "token_id": "tok-1",
                "type": "llm",
                "provider": "openai",
                "is_active": True,
                "scope_type": "global",
            },
            {
                "token_id": "tok-2",
                "type": "github",
                "provider": "github",
                "is_active": True,
                "scope_type": "global",
            },
        ]

        result = runner.invoke(admin_cli, ["token", "list"])

        assert result.exit_code == 0
        mock_admin_api_client.admin_list_tokens.assert_called_once()
        assert "tok-1" in result.output
        assert "openai" in result.output

    def test_auth_audit_logs_calls_api_client(self, runner, mock_admin_api_client):
        """Test audit logs command calls API client."""
        mock_admin_api_client.admin_auth_audit_logs.return_value = [
            {"user_id": "alice", "action": "login", "timestamp": "2026-02-23T10:00:00Z"},
            {"user_id": "bob", "action": "create_session", "timestamp": "2026-02-23T11:00:00Z"},
        ]

        result = runner.invoke(admin_cli, ["audit", "logs", "--user", "alice"])

        assert result.exit_code == 0
        mock_admin_api_client.admin_auth_audit_logs.assert_called_once()
        assert "alice" in result.output
        assert "login" in result.output

    def test_feedback_stats_calls_api_client(self, runner, mock_admin_api_client):
        """Test feedback stats command calls API client."""
        mock_admin_api_client.admin_feedback_stats.return_value = {
            "total": 100,
            "positive": 80,
            "negative": 20,
            "avg_rating": 4.2,
        }

        result = runner.invoke(admin_cli, ["feedback", "stats"])

        assert result.exit_code == 0
        mock_admin_api_client.admin_feedback_stats.assert_called_once()
        assert "100" in result.output
        assert "4.2" in result.output


class TestEdgeChatE2E:
    """Test chat command uses edge execution path."""

    def test_chat_calls_edge_turn(self, runner, mock_api_client):
        """Chat command calls _run_edge_turn, not chat_stream."""
        from unittest.mock import patch
        from cli.edge_chat_loop import ChatLoopResult

        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.create_session.return_value = {"session_id": "sess-123"}
        mock_api_client.close_session.return_value = {}

        call_count = 0

        async def fake_edge(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            return ChatLoopResult(text="ok")

        with patch("cli.mo_agent_api._run_edge_turn", new=fake_edge):
            runner.invoke(
                agent_cli,
                ["chat", "--user-id", "alice"],
                input="test message\nexit\n",
            )
            assert call_count > 0
            assert not mock_api_client.chat_stream.called

    def test_chat_debug_shows_traceback(self, runner, mock_api_client):
        """--debug flag prints full traceback on error."""
        from unittest.mock import patch

        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.create_session.return_value = {"session_id": "sess-123"}
        mock_api_client.close_session.return_value = {}

        async def raise_boom(*args, **kwargs):
            raise ValueError("boom")

        with patch("cli.mo_agent_api._run_edge_turn", new=raise_boom):
            result = runner.invoke(
                agent_cli,
                ["chat", "--debug"],
                input="test message\nexit\n",
            )
            assert "Traceback" in result.output
            assert "boom" in result.output

    def test_session_info_persists_across_turns(self, runner, mock_api_client):
        """session_info is reused across turns so turn count accumulates correctly."""
        from unittest.mock import patch
        from cli.edge_chat_loop import ChatLoopResult

        mock_api_client.ensure_authenticated.return_value = True
        mock_api_client.create_session.return_value = {"session_id": "sess-123"}
        mock_api_client.close_session.return_value = {}

        captured_session_infos = []

        async def fake_edge_turn(*args, session_info=None, **kwargs):
            captured_session_infos.append(session_info)
            return ChatLoopResult(text="ok", usage={"prompt_tokens": 100, "completion_tokens": 20})

        with patch("cli.mo_agent_api._run_edge_turn", new=fake_edge_turn):
            runner.invoke(
                agent_cli,
                ["chat", "--user-id", "alice"],
                input="first message\nsecond message\nexit\n",
            )

        assert len(captured_session_infos) == 2
        # Same dict object reused across turns
        assert captured_session_infos[0] is captured_session_infos[1]
        # turn incremented after each turn
        assert captured_session_infos[1]["turn"] == 2
        # token usage updated
        assert captured_session_infos[1]["prompt_tokens"] == 100
        assert captured_session_infos[1]["completion_tokens"] == 20


class TestLogoutCommand:
    """Test logout CLI command."""

    def test_logout_calls_api_client(self, runner, mock_api_client):
        mock_api_client.logout.return_value = None
        result = runner.invoke(agent_cli, ["logout"])
        assert "Logged out" in result.output
        mock_api_client.logout.assert_called_once()

    def test_logout_handles_error(self, runner, mock_api_client):
        mock_api_client.logout.side_effect = Exception("no file")
        result = runner.invoke(agent_cli, ["logout"])
        assert "no file" in result.output
