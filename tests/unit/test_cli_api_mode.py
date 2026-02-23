"""Tests for API-mode CLI commands."""

import pytest
from click.testing import CliRunner
from unittest.mock import patch, MagicMock, AsyncMock

from cli.mo_agent_api import cli as agent_cli
from cli.mo_admin_api import cli as admin_cli


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

    def test_register_success(self, runner):
        """Test successful registration."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.register.return_value = {"email": "bob@example.com"}
            
            result = runner.invoke(
                agent_cli,
                ["register"],
                input="bob@example.com\npassword\npassword\nbob\n"
            )
            assert result.exit_code == 0
            assert "✅ Registered" in result.output

    def test_whoami_authenticated(self, runner):
        """Test whoami when authenticated."""
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.get_current_user.return_value = {
                "email": "alice@example.com",
                "user_id": "u_alice"
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
                {
                    "session_id": "s1",
                    "user_id": "alice",
                    "status": "active",
                    "event_count": 5
                }
            ]
            
            result = runner.invoke(agent_cli, ["session", "list"])
            assert result.exit_code == 0
            assert "s1" in result.output

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
                "event_count": 5
            }
            
            result = runner.invoke(agent_cli, ["session", "show", "s1"])
            assert result.exit_code == 0
            assert "s1" in result.output

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
                    "description": "Summarize text"
                }
            ]
            
            result = runner.invoke(agent_cli, ["skill", "list"])
            assert result.exit_code == 0
            assert "summarize" in result.output

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
            mock_client.replay_session.return_value = {
                "status": "success",
                "events_replayed": 5
            }
            
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

    def test_init_database(self, runner):
        """Test database initialization."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_init.return_value = {"tables_created": 15}
            
            result = runner.invoke(admin_cli, ["init"])
            assert result.exit_code == 0
            assert "✅ Database initialized" in result.output

    def test_token_create(self, runner):
        """Test token creation."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_create_token.return_value = {
                "token_id": "t1",
                "provider": "openai",
                "scope_type": "global"
            }
            
            result = runner.invoke(
                admin_cli,
                ["token", "create", "--type", "llm", "--provider", "openai"],
                input="secret_key\n"
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
                    "is_active": True
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

    def test_audit_logs(self, runner):
        """Test audit logs command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_audit_logs.return_value = [
                {
                    "created_at": "2026-02-23T10:00:00",
                    "action": "token_created",
                    "user_id": "admin"
                }
            ]
            
            result = runner.invoke(admin_cli, ["audit", "logs"])
            assert result.exit_code == 0
            assert "token_created" in result.output

    def test_audit_logs_empty(self, runner):
        """Test audit logs when no logs."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.admin_audit_logs.return_value = []
            
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
                "avg_rating": 4.2
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
            mock_client.admin_feedback_export.return_value = {
                "count": 50,
                "data": []
            }
            
            result = runner.invoke(admin_cli, ["feedback", "export"])
            assert result.exit_code == 0
            assert "✅ Exported" in result.output

    def test_whoami(self, runner):
        """Test whoami command."""
        with patch("cli.mo_admin_api.SyncAPIClient") as mock_client_class:
            mock_client = MagicMock()
            mock_client_class.return_value = mock_client
            mock_client.get_current_user.return_value = {
                "email": "admin@example.com",
                "role": "admin"
            }
            
            result = runner.invoke(admin_cli, ["whoami"])
            assert result.exit_code == 0
            assert "admin@example.com" in result.output
