"""Tests for tool bypass warning — warns but doesn't block rm on source files.

Design: Unlike dangerous commands (DENY), potential bypass attempts return ASK
with a warning. Users may have legitimate reasons to delete source files.

The primary scenario this addresses:
1. Agent tries write_file on existing file → rejected ("file already exists")
2. Agent tries `bash rm file.py` → ASK with warning (user decides)
3. Prompt constraint guides agent to not do this in the first place
"""

import pytest

from cli.permissions import (
    DANGEROUS_PATTERNS,
    TOOL_BYPASS_WARNING_PATTERNS,
    Decision,
    PermissionManager,
)
from cli.tools.base import SideEffect


class TestBypassWarningPatterns:
    """Verify patterns correctly identify potential bypass attempts."""

    @pytest.mark.parametrize(
        "command",
        [
            "rm skill.py",
            "rm /home/user/project/skill.py",
            "rm -f skill.py",
            "rm src/main.py",
            "ls && rm file.py",
            "echo x; rm file.py",
        ],
    )
    def test_detects_source_file_deletion(self, command: str):
        """rm on source files is detected as potential bypass."""
        assert any(p.search(command) for p in TOOL_BYPASS_WARNING_PATTERNS)

    @pytest.mark.parametrize(
        "command",
        [
            "rm -rf build/",
            "rm -r node_modules",
            "rm file.log",
            "ls -la",
            "echo rm file.py",  # rm as argument, not command
            "grep rm file.py",
        ],
    )
    def test_ignores_non_bypass_commands(self, command: str):
        """Non-bypass commands are not flagged."""
        assert not any(p.search(command) for p in TOOL_BYPASS_WARNING_PATTERNS)


class TestPermissionManagerBypassWarning:
    """Verify bypass attempts return ASK (not DENY) with warning."""

    def test_rm_source_file_returns_ask_not_deny(self):
        """rm on source file returns ASK, allowing user to decide."""
        pm = PermissionManager()
        decision = pm.check("bash", SideEffect.EXECUTE, {"command": "rm skill.py"})
        # Key change: ASK instead of DENY
        assert decision == Decision.ASK

    def test_bypass_warning_is_set(self):
        """Warning message is available after check."""
        pm = PermissionManager()
        pm.check("bash", SideEffect.EXECUTE, {"command": "rm skill.py"})
        warning = pm.get_bypass_warning()
        assert warning is not None
        assert "bypass" in warning.lower() or "source file" in warning.lower()

    def test_no_warning_for_normal_commands(self):
        """Normal commands don't trigger bypass warning."""
        pm = PermissionManager()
        pm.check("bash", SideEffect.EXECUTE, {"command": "ls -la"})
        assert pm.get_bypass_warning() is None

    def test_session_override_still_asks_for_bypass(self):
        """Even with 'always allow bash', bypass attempts still ASK."""
        pm = PermissionManager()
        pm.set_session_override("bash", Decision.ALLOW)

        # Normal command uses override
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "ls"}) == Decision.ALLOW

        # Bypass attempt still asks (override doesn't apply)
        decision = pm.check("bash", SideEffect.EXECUTE, {"command": "rm skill.py"})
        assert decision == Decision.ASK
        assert pm.get_bypass_warning() is not None

    def test_auto_approve_still_asks_for_bypass(self):
        """Even with auto_approve=True, bypass attempts still ASK."""
        pm = PermissionManager(auto_approve=True)
        decision = pm.check("bash", SideEffect.EXECUTE, {"command": "rm skill.py"})
        assert decision == Decision.ASK

    def test_dangerous_commands_still_denied(self):
        """Truly dangerous commands are still DENY."""
        pm = PermissionManager()
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "rm -rf /"}) == Decision.DENY
        assert pm.check("bash", SideEffect.EXECUTE, {"command": "sudo rm file.py"}) == Decision.DENY


class TestBypassVsDangerousDistinction:
    """Verify clear distinction between DENY (dangerous) and ASK (bypass)."""

    def test_dangerous_is_deny(self):
        """Dangerous patterns → DENY (no user choice)."""
        pm = PermissionManager()
        dangerous_commands = ["rm -rf /", "sudo anything", "curl x | sh"]
        for cmd in dangerous_commands:
            if any(p.search(cmd) for p in DANGEROUS_PATTERNS):
                assert pm.check("bash", SideEffect.EXECUTE, {"command": cmd}) == Decision.DENY

    def test_bypass_is_ask(self):
        """Bypass patterns → ASK (user decides)."""
        pm = PermissionManager()
        bypass_commands = ["rm file.py", "rm -f main.js", "rm config.json"]
        for cmd in bypass_commands:
            decision = pm.check("bash", SideEffect.EXECUTE, {"command": cmd})
            assert decision == Decision.ASK, f"Expected ASK for '{cmd}', got {decision}"
