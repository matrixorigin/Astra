"""Tests for rich permission prompts."""

from cli.permissions import PermissionManager, Decision
from cli.tools.base import SideEffect


class TestFormatPrompt:
    def test_bash_contains_tool_name(self):
        pm = PermissionManager()
        result = pm.format_prompt("bash", {"command": "ls -la"})
        assert "bash" in result

    def test_write_file_contains_path(self):
        pm = PermissionManager()
        result = pm.format_prompt("write_file", {"path": "main.py"})
        assert "main.py" in result

    def test_generic_tool(self):
        pm = PermissionManager()
        result = pm.format_prompt("custom_tool", {"key": "value"})
        assert "custom_tool" in result


class TestFormatPromptPlain:
    def test_plain_no_markup(self):
        pm = PermissionManager()
        result = pm.format_prompt_plain("bash", {"command": "echo hi"})
        assert "bash" in result
        assert "[bold]" not in result  # no rich markup


class TestPermissionCheckUnchanged:
    """Verify core permission logic is unchanged."""

    def test_read_auto_allow(self):
        pm = PermissionManager()
        assert pm.check("read_file", SideEffect.READ, {}) == Decision.ALLOW

    def test_write_asks(self):
        pm = PermissionManager()
        assert pm.check("write_file", SideEffect.WRITE, {}) == Decision.ASK

    def test_execute_asks(self):
        pm = PermissionManager()
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "ls"}) == Decision.ASK

    def test_dangerous_denied(self):
        pm = PermissionManager()
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "sudo rm -rf /"}) == Decision.DENY

    def test_auto_approve(self):
        pm = PermissionManager(auto_approve=True)
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "ls"}) == Decision.ALLOW

    def test_session_override(self):
        pm = PermissionManager()
        pm.set_session_override("bash", Decision.ALLOW)
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "ls"}) == Decision.ALLOW
