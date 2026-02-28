"""End-to-end flow tests for the new CLI UX layer."""

from io import StringIO
from unittest.mock import MagicMock, patch

import pytest
from click.testing import CliRunner
from rich.console import Console

from cli.mo_agent_api import cli as agent_cli, SLASH_COMMANDS, cmd_help, cmd_version


class TestClickSubcommandsStillWork:
    """Existing click subcommands must still work after refactoring."""

    def test_login(self):
        runner = CliRunner()
        with patch("cli.mo_agent_api.SyncAPIClient") as mock:
            mock.return_value.login.return_value = {"email": "a@b.com"}
            result = runner.invoke(agent_cli, ["login"], input="user\npass\n")
            assert result.exit_code == 0
            assert "Logged in" in result.output

    def test_whoami(self):
        runner = CliRunner()
        with patch("cli.mo_agent_api.SyncAPIClient") as mock:
            mock.return_value.get_current_user.return_value = {
                "email": "a@b.com", "user_id": "u1"
            }
            result = runner.invoke(agent_cli, ["whoami"])
            assert result.exit_code == 0
            assert "a@b.com" in result.output

    def test_session_list(self):
        runner = CliRunner()
        with patch("cli.mo_agent_api.SyncAPIClient") as mock:
            inst = mock.return_value
            inst.ensure_authenticated.return_value = True
            inst.list_sessions.return_value = {
                "sessions": [{"session_id": "s1", "status": "active", "event_count": 3}],
                "total": 1,
            }
            result = runner.invoke(agent_cli, ["session", "list"])
            assert result.exit_code == 0
            assert "s1" in result.output

    def test_doctor_subcommand(self):
        runner = CliRunner()
        with patch("cli.mo_agent_api.SyncAPIClient") as mock:
            mock.return_value.base_url = "http://localhost:8000"
            mock.return_value.ensure_authenticated.return_value = True
            result = runner.invoke(agent_cli, ["doctor"])
            assert result.exit_code == 0


class TestSlashCommandDispatch:
    """Verify slash command dispatch works correctly."""

    def test_all_commands_in_registry(self):
        expected = {"/help", "/model", "/session", "/clear", "/verbose",
                    "/compact", "/history", "/copy", "/doctor", "/version"}
        assert expected.issubset(set(SLASH_COMMANDS.keys()))

    def test_help_output(self):
        buf = StringIO()
        console = Console(file=buf, force_terminal=True, width=80)
        cmd_help(console=console)
        output = buf.getvalue()
        # All user-facing commands should appear in help
        for cmd in ["/help", "/model", "/session", "/clear", "/exit", "/verbose", "/compact"]:
            assert cmd in output, f"{cmd} missing from /help output"

    def test_version_output(self):
        buf = StringIO()
        console = Console(file=buf, force_terminal=True, width=80)
        cmd_version(console=console)
        output = buf.getvalue()
        assert "mo-agent" in output
        assert "Python" in output


class TestRendererSelection:
    """Verify correct renderer is chosen based on TTY."""

    def test_rich_renderer_is_tty(self):
        from cli.ui.renderer import RichRenderer
        r = RichRenderer()
        assert hasattr(r, "begin_response")
        assert hasattr(r, "end_response")

    def test_simple_renderer_non_tty(self):
        from cli.ui.renderer import SimpleRenderer
        r = SimpleRenderer()
        # SimpleRenderer should not have begin_response/end_response
        assert not hasattr(r, "begin_response")


class TestStateManagement:
    """Verify state dict is properly managed."""

    def test_cmd_clear_updates_state(self):
        buf = StringIO()
        console = Console(file=buf, force_terminal=True, width=80)
        client = MagicMock()
        client.create_session.return_value = {"session_id": "new_ses"}
        state = {"session_id": "old_ses"}
        from cli.mo_agent_api import cmd_clear
        cmd_clear(console=console, client=client, state=state)
        assert state["session_id"] == "new_ses"

    def test_cmd_model_updates_state(self):
        buf = StringIO()
        console = Console(file=buf, force_terminal=True, width=80)
        client = MagicMock()
        client.admin_list_models.return_value = [
            {"name": "gpt-4", "provider": "openai", "is_active": True}
        ]
        state = {"selected_model": None}
        from cli.mo_agent_api import cmd_model
        cmd_model(console=console, client=client, cmd_arg="gpt-4", state=state)
        assert state["selected_model"] == "gpt-4"

    def test_cmd_verbose_compact_toggle(self):
        buf = StringIO()
        console = Console(file=buf, force_terminal=True, width=80)
        from cli.ui.status_bar import StatusBar
        from cli.mo_agent_api import cmd_verbose, cmd_compact
        sb = StatusBar()
        assert not sb.verbose
        cmd_verbose(console=console, status_bar=sb)
        assert sb.verbose
        cmd_compact(console=console, status_bar=sb)
        assert not sb.verbose


class TestAuthErrorInRepl:
    """AuthenticationError triggers re-login or clean exit."""

    def test_auth_error_non_tty_exits_cleanly(self):
        """Non-TTY: auth error prints message and exits (no re-login prompt)."""
        from cli.api_client import AuthenticationError

        runner = CliRunner()
        with patch("cli.mo_agent_api.SyncAPIClient") as mock_cls:
            client = mock_cls.return_value
            client.ensure_authenticated.return_value = True
            client.get_current_user.return_value = {"username": "alice"}
            client.create_session.return_value = {"session_id": "s1"}
            client.close_session.return_value = {}

            with patch("cli.mo_agent_api._run_edge_turn", side_effect=AuthenticationError("expired")):
                result = runner.invoke(agent_cli, ["chat"], input="hello\n")

            assert "Session expired" in result.output
            assert result.exit_code == 0
