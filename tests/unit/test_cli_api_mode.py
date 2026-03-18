"""Tests for API-mode CLI commands.

Note: SyncAPIClient uses __getattr__ to proxy methods to the underlying
APIClient, so spec=SyncAPIClient would block access to proxied methods
(login, list_skills, etc.).  We use plain MagicMock and rely on
assert_called_once_with() to catch parameter errors instead.
"""

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from click.testing import CliRunner

from cli.mo_admin_api import cli as admin_cli
from cli.mo_agent_api import cli as agent_cli


def _make_chat_mock(mock_client):
    """Configure a MagicMock SyncAPIClient for chat command tests."""
    mock_client._run.side_effect = lambda coro: asyncio.run(coro)
    mock_client._ensure_client.return_value = mock_client
    return mock_client


@pytest.fixture
def runner():
    return CliRunner()


class TestAgentCLI:
    """Test mo-agent API CLI."""

    def test_login_success(self, runner):
        """Test successful login."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.login.return_value = {"email": "alice@example.com"}

            result = runner.invoke(agent_cli, ["login"], input="alice@example.com\npassword\n")
            assert result.exit_code == 0
            assert "✅ Logged in" in result.output
            mock_client.login.assert_called_once_with("alice@example.com", "password")

    def test_register_success(self, runner):
        """Test successful registration."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.register.return_value = {"email": "bob@example.com"}

            result = runner.invoke(
                agent_cli, ["register"], input="bob@example.com\npassword\npassword\nbob\n"
            )
            assert result.exit_code == 0
            assert "✅ Registered" in result.output
            mock_client.register.assert_called_once_with(
                "bob",
                "password",
                "bob@example.com",
            )

    def test_whoami_authenticated(self, runner):
        """Test whoami when authenticated."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.get_current_user.return_value = {
                "email": "alice@example.com",
                "user_id": "u_alice",
            }

            result = runner.invoke(agent_cli, ["whoami"])
            assert result.exit_code == 0
            assert "alice@example.com" in result.output

    def test_session_list(self, runner):
        """Test session list command."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.list_sessions.return_value = [
                {"session_id": "s1", "user_id": "alice", "status": "active", "event_count": 5}
            ]

            result = runner.invoke(agent_cli, ["session", "list"])
            assert result.exit_code == 0
            assert "s1" in result.output
            mock_client.list_sessions.assert_called_once_with(agent_id=None, status=None, limit=20)

    def test_session_list_empty(self, runner):
        """Test session list when no sessions."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.list_sessions.return_value = []

            result = runner.invoke(agent_cli, ["session", "list"])
            assert result.exit_code == 0
            assert "No sessions found" in result.output

    def test_session_show(self, runner):
        """Test session show command."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.get_session.return_value = {
                "session_id": "s1",
                "user_id": "alice",
                "status": "active",
                "event_count": 5,
            }

            result = runner.invoke(agent_cli, ["session", "show", "s1"])
            assert result.exit_code == 0
            assert "s1" in result.output
            mock_client.get_session.assert_called_once_with("s1")

    def test_skill_list(self, runner):
        """Test skill list command."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.list_skills.return_value = [
                {
                    "skill_name": "summarize",
                    "version": "1.0",
                    "is_active": True,
                    "description": "Summarize text",
                }
            ]

            result = runner.invoke(agent_cli, ["skill", "list"])
            assert result.exit_code == 0
            assert "summarize" in result.output
            mock_client.list_skills.assert_called_once_with()

    def test_skill_list_empty(self, runner):
        """Test skill list when no skills."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.list_skills.return_value = []

            result = runner.invoke(agent_cli, ["skill", "list"])
            assert result.exit_code == 0
            assert "No skills found" in result.output

    def test_replay_session(self, runner):
        """Test replay command."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.replay_session.return_value = {"status": "success", "events_replayed": 5}

            result = runner.invoke(agent_cli, ["replay", "s1"])
            assert result.exit_code == 0
            assert "5" in result.output


class TestAdminCLI:
    """Test mo-admin API CLI."""

    def test_login_success(self, runner):
        """Test admin login."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.login.return_value = {"email": "admin@example.com"}

            result = runner.invoke(admin_cli, ["login"], input="admin@example.com\npassword\n")
            assert result.exit_code == 0
            assert "✅ Logged in" in result.output
            mock_client.login.assert_called_once_with(
                "admin@example.com",
                "password",
            )

    def test_init_database(self, runner):
        """Test database initialization."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_init.return_value = {"tables_created": 15}

            result = runner.invoke(admin_cli, ["init"])
            assert result.exit_code == 0
            assert "✅ Database initialized" in result.output
            mock_client.admin_init.assert_called_once_with()

    def test_token_create(self, runner):
        """Test token creation."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_create_token.return_value = {
                "token_id": "t1",
                "provider": "openai",
                "scope_type": "global",
            }

            result = runner.invoke(
                admin_cli,
                ["token", "create", "--type", "llm", "--provider", "openai"],
                input="secret_key\n",
            )
            assert result.exit_code == 0
            assert "✅ Token created" in result.output

    def test_token_list(self, runner):
        """Test token listing."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_list_tokens.return_value = [
                {
                    "token_id": "t1",
                    "type": "llm",
                    "provider": "openai",
                    "scope_type": "global",
                    "is_active": True,
                }
            ]

            result = runner.invoke(admin_cli, ["token", "list"])
            assert result.exit_code == 0
            assert "openai" in result.output

    def test_token_list_empty(self, runner):
        """Test token list when no tokens."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_list_tokens.return_value = []

            result = runner.invoke(admin_cli, ["token", "list"])
            assert result.exit_code == 0
            assert "No tokens found" in result.output

    def test_auth_audit_logs(self, runner):
        """Test audit logs command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_auth_audit_logs.return_value = [
                {"created_at": "2026-02-23T10:00:00", "action": "token_created", "user_id": "admin"}
            ]

            result = runner.invoke(admin_cli, ["audit", "logs"])
            assert result.exit_code == 0
            assert "token_created" in result.output

    def test_auth_audit_logs_empty(self, runner):
        """Test audit logs when no logs."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_auth_audit_logs.return_value = []

            result = runner.invoke(admin_cli, ["audit", "logs"])
            assert result.exit_code == 0
            assert "No logs found" in result.output

    def test_feedback_stats(self, runner):
        """Test feedback stats command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_feedback_stats.return_value = {
                "total": 100,
                "positive": 80,
                "negative": 10,
                "avg_rating": 4.2,
            }

            result = runner.invoke(admin_cli, ["feedback", "stats"])
            assert result.exit_code == 0
            assert "100" in result.output
            assert "4.2" in result.output

    def test_feedback_export(self, runner):
        """Test feedback export command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            # API returns async job info, not data
            mock_client.admin_feedback_export.return_value = {
                "job_id": "job-123",
                "status": "queued",
                "download_url": None,
            }

            result = runner.invoke(admin_cli, ["feedback", "export"])
            assert result.exit_code == 0
            assert "Export job created" in result.output or "Export ready" in result.output

    def test_whoami(self, runner):
        """Test whoami command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.get_current_user.return_value = {
                "email": "admin@example.com",
                "role": "admin",
            }

            result = runner.invoke(admin_cli, ["whoami"])
            assert result.exit_code == 0
            assert "admin@example.com" in result.output

    def test_user_grant_role_success(self, runner):
        """Test granting role to user."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = True
            mock_client.admin_grant_role.return_value = {
                "username": "alice",
                "role_name": "mo_agent_admin",
                "message": "Role granted successfully",
            }

            result = runner.invoke(admin_cli, ["user", "grant-role", "alice", "mo_agent_admin"])
            assert result.exit_code == 0
            assert "✅" in result.output
            assert "granted" in result.output.lower()

    def test_user_grant_role_not_authenticated(self, runner):
        """Test granting role requires authentication."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = False

            result = runner.invoke(admin_cli, ["user", "grant-role", "alice", "mo_agent_admin"])
            assert result.exit_code == 1
            assert "login first" in result.output.lower()

    def test_user_grant_role_session_expired(self, runner):
        """Test granting role with expired session shows expiry message."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = "session_expired"

            result = runner.invoke(admin_cli, ["user", "grant-role", "alice", "mo_agent_admin"])
            assert result.exit_code == 1
            assert "expired" in result.output.lower()

    def test_user_revoke_role_success(self, runner):
        """Test revoking role from user."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = True
            mock_client.admin_revoke_role.return_value = {
                "username": "alice",
                "role_name": "mo_agent_admin",
                "message": "Role revoked successfully",
            }

            result = runner.invoke(admin_cli, ["user", "revoke-role", "alice", "mo_agent_admin"])
            assert result.exit_code == 0
            assert "✅" in result.output
            assert "revoked" in result.output.lower()

    def test_user_grant_role_invalid_role(self, runner):
        """Test granting invalid role name."""
        result = runner.invoke(admin_cli, ["user", "grant-role", "alice", "invalid_role"])
        assert result.exit_code != 0  # Click validation should fail


# ============================================================================
# Tests: CLI edge mode integration
# ============================================================================


class TestCLIEdgeMode:
    """Test edge mode integration in mo-agent chat."""

    def test_run_edge_turn_registers_all_tools(self):
        """_run_edge_turn initializes router with all 10 tools."""
        from unittest.mock import AsyncMock, patch

        from cli.mo_agent_api import _run_edge_turn

        captured = {}

        async def fake_edge_chat_loop(user_input, api, router, perms, **kwargs):
            captured["tools"] = list(router._tools.keys())
            captured["auto_approve"] = perms._auto_approve
            captured["kwargs"] = kwargs

        mock_api = AsyncMock()
        mock_api.get_current_user = AsyncMock(
            return_value={"user_id": "test-user-id", "username": "testuser"}
        )
        mock_api.get_introspection_skills = AsyncMock(return_value={"cloud": [], "installed": []})

        with (
            patch("cli.edge_chat_loop.edge_chat_loop", fake_edge_chat_loop),
            patch("core.skills.loader.SkillLoader.discover", return_value=[]),
        ):
            asyncio.run(_run_edge_turn("test", mock_api, "ses_1", "gpt-4", "agent-1", True))

        expected_tools = {
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "bash",
            "git_status",
            "git_diff",
            "git_log",
            "grep",
            "glob",
            "get_agent_info",
            "reflect",
            "find_skills",
            "memory_retrieve",
            "memory_search",
            "memory_profile",
            "memory_store",
            "memory_correct",
            "memory_purge",
            "memory_program",
            "skill_config_wizard",
            "set_skill_setting",
            "bind_skill_resource",
            "validate_skill_config",
        }
        assert set(captured["tools"]) == expected_tools
        assert captured["auto_approve"] is True
        assert captured["kwargs"]["session_id"] == "ses_1"
        assert captured["kwargs"]["model"] == "gpt-4"
        assert captured["kwargs"]["agent_id"] == "agent-1"

    def test_run_edge_turn_filters_memory_tools_by_backend_capabilities(self):
        from unittest.mock import AsyncMock, patch

        from cli.mo_agent_api import _run_edge_turn
        from core.memory.backends.factory import MemoryBackendCapabilities

        captured = {}

        async def fake_edge_chat_loop(user_input, api, router, perms, **kwargs):
            captured["tools"] = list(router._tools.keys())

        mock_api = AsyncMock()
        mock_api.get_current_user = AsyncMock(
            return_value={"user_id": "test-user-id", "username": "testuser"}
        )
        mock_api.get_introspection_skills = AsyncMock(return_value={"cloud": [], "installed": []})
        capabilities = MemoryBackendCapabilities(
            backend_name="test",
            supported_tools=("memory_retrieve", "memory_profile"),
            supported_context_modes=("profile_only", "retrieve"),
        )

        with (
            patch("cli.edge_chat_loop.edge_chat_loop", fake_edge_chat_loop),
            patch("core.skills.loader.SkillLoader.discover", return_value=[]),
            patch("core.memory.backends.get_memory_backend_capabilities", return_value=capabilities),
        ):
            asyncio.run(_run_edge_turn("test", mock_api, "ses_1", "gpt-4", "agent-1", True))

        assert "memory_retrieve" in captured["tools"]
        assert "memory_profile" in captured["tools"]
        assert "memory_search" not in captured["tools"]
        assert "memory_store" not in captured["tools"]
        assert "memory_program" not in captured["tools"]

    def test_chat_uses_edge_path(self, runner):
        """Chat always uses _run_edge_turn."""
        from cli.edge_chat_loop import ChatLoopResult

        call_count = 0

        async def fake_edge(*args, **kwargs):
            nonlocal call_count
            call_count += 1
            return ChatLoopResult(text="ok")

        with (
            patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class,
            patch("cli.mo_agent_api._run_edge_turn", new=fake_edge),
        ):
            mock_client = _make_chat_mock(MagicMock())
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = True
            mock_client.get_current_user.return_value = {"username": "alice"}
            mock_client.create_session.return_value = {"session_id": "ses_1"}

            runner.invoke(agent_cli, ["chat"], input="hello\n/exit\n")
            assert call_count > 0

    def test_auto_approve_flag_passed(self, runner):
        """--auto-approve flag is forwarded to _run_edge_turn."""
        from cli.edge_chat_loop import ChatLoopResult

        captured_kwargs: dict = {}

        async def fake_edge(*args, **kwargs):
            captured_kwargs.update(kwargs)
            # auto_approve is the 6th positional arg
            captured_kwargs["auto_approve"] = args[5] if len(args) > 5 else None
            return ChatLoopResult(text="ok")

        with (
            patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class,
            patch("cli.mo_agent_api._run_edge_turn", new=fake_edge),
        ):
            mock_client = _make_chat_mock(MagicMock())
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = True
            mock_client.get_current_user.return_value = {"username": "alice"}
            mock_client.create_session.return_value = {"session_id": "ses_1"}

            runner.invoke(agent_cli, ["chat", "--auto-approve"], input="hello\n/exit\n")
            assert captured_kwargs.get("auto_approve") is True

    def test_debug_flag_shows_traceback(self, runner):
        """--debug prints full traceback on error."""

        async def raise_boom(*args, **kwargs):
            raise ValueError("test boom")

        with (
            patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class,
            patch("cli.mo_agent_api._run_edge_turn", new=raise_boom),
        ):
            mock_client = _make_chat_mock(MagicMock())
            mock_client_class.return_value = mock_client
            mock_client.ensure_authenticated.return_value = True
            mock_client.get_current_user.return_value = {"username": "alice"}
            mock_client.create_session.return_value = {"session_id": "ses_1"}

            result = runner.invoke(agent_cli, ["chat", "--debug"], input="hello\n/exit\n")
            assert "Traceback" in result.output
            assert "test boom" in result.output
