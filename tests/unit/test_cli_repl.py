"""Tests for slash command handlers and chat REPL integration."""

from io import StringIO
from unittest.mock import MagicMock

from rich.console import Console

from cli.mo_agent_api import (
    SLASH_COMMANDS, cmd_help, cmd_model, cmd_session, cmd_clear,
    cmd_verbose, cmd_compact, cmd_history, cmd_copy, cmd_version,
)


def _console() -> tuple[Console, StringIO]:
    buf = StringIO()
    return Console(file=buf, force_terminal=True, width=80), buf


class TestSlashCommandRegistry:
    def test_all_handlers_callable(self):
        for name, handler in SLASH_COMMANDS.items():
            assert callable(handler), f"{name} handler not callable"

    def test_expected_commands_present(self):
        expected = {"/help", "/model", "/session", "/clear", "/verbose",
                    "/compact", "/history", "/copy", "/doctor", "/version"}
        assert expected.issubset(set(SLASH_COMMANDS.keys()))


class TestCmdHelp:
    def test_output_contains_commands(self):
        console, buf = _console()
        cmd_help(console=console)
        output = buf.getvalue()
        assert "/help" in output
        assert "/model" in output
        assert "/exit" in output
        assert "/verbose" in output


class TestCmdModel:
    def test_list_models(self):
        console, buf = _console()
        client = MagicMock()
        client.admin_list_models.return_value = [
            {"name": "gpt-4", "provider": "openai", "is_active": True},
            {"name": "claude", "provider": "anthropic", "is_active": True},
        ]
        state = {"selected_model": "gpt-4"}
        cmd_model(console=console, client=client, state=state)
        output = buf.getvalue()
        assert "gpt-4" in output
        assert "claude" in output

    def test_select_model(self):
        console, buf = _console()
        client = MagicMock()
        client.admin_list_models.return_value = [
            {"name": "gpt-4", "provider": "openai", "is_active": True},
        ]
        state = {"selected_model": None}
        cmd_model(console=console, client=client, cmd_arg="gpt-4", state=state)
        assert state["selected_model"] == "gpt-4"

    def test_select_unknown_model(self):
        console, buf = _console()
        client = MagicMock()
        client.admin_list_models.return_value = [
            {"name": "gpt-4", "provider": "openai", "is_active": True},
        ]
        state = {"selected_model": None}
        cmd_model(console=console, client=client, cmd_arg="nonexistent", state=state)
        assert state["selected_model"] is None
        assert "Unknown" in buf.getvalue()


class TestCmdSession:
    def test_shows_session_info(self):
        console, buf = _console()
        state = {"selected_model": "gpt-4"}
        cmd_session(console=console, session_id="ses_123", username="alice", state=state)
        output = buf.getvalue()
        assert "ses_123" in output
        assert "alice" in output


class TestCmdClear:
    def test_creates_new_session(self):
        console, buf = _console()
        client = MagicMock()
        client.create_session.return_value = {"session_id": "ses_new"}
        state = {"session_id": "ses_old"}
        cmd_clear(console=console, client=client, state=state)
        assert state["session_id"] == "ses_new"
        client.close_session.assert_called_once_with("ses_old")


class TestCmdVerboseCompact:
    def test_verbose_enables(self):
        console, buf = _console()
        sb = MagicMock()
        cmd_verbose(console=console, status_bar=sb)
        assert sb.verbose is True

    def test_compact_disables(self):
        console, buf = _console()
        sb = MagicMock()
        cmd_compact(console=console, status_bar=sb)
        assert sb.verbose is False


class TestCmdHistory:
    def test_empty_history(self):
        console, buf = _console()
        cmd_history(console=console, state={"turn_history": []})
        assert "No history" in buf.getvalue()

    def test_with_history(self):
        console, buf = _console()
        state = {"turn_history": [
            {"role": "user", "preview": "hello"},
            {"role": "assistant", "preview": "hi there"},
        ]}
        cmd_history(console=console, state=state)
        output = buf.getvalue()
        assert "hello" in output
        assert "hi there" in output


class TestCmdVersion:
    def test_shows_version(self):
        console, buf = _console()
        cmd_version(console=console)
        output = buf.getvalue()
        assert "mo-agent" in output
        assert "Python" in output
