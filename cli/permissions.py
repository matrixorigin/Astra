"""Permission system for edge tool execution.

Default policy:
  READ tools → auto-allow
  WRITE tools → ask user
  EXECUTE tools → ask user
  Dangerous commands → deny
"""

import re
from enum import Enum
from typing import Any

from cli.tools.base import SideEffect

# Commands that are always denied
DANGEROUS_PATTERNS = [
    re.compile(r"\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?/\s*$"),  # rm -rf /
    re.compile(r"\bsudo\b"),
    re.compile(r"\bmkfs\b"),
    re.compile(r"\bdd\s+.*of=/dev/"),
    re.compile(r"\b:(){ :\|:& };:"),  # fork bomb
    re.compile(r"\bchmod\s+777\s+/"),
    re.compile(r"\bcurl\b.*\|\s*(ba)?sh"),  # curl | sh
]


class Decision(str, Enum):
    ALLOW = "allow"
    DENY = "deny"
    ASK = "ask"


class PermissionManager:
    """Manages tool execution permissions with session-level overrides."""

    def __init__(self, auto_approve: bool = False) -> None:
        self._auto_approve = auto_approve
        # Session overrides: tool_name → Decision
        self._session_overrides: dict[str, Decision] = {}

    def check(self, tool_name: str, side_effect: SideEffect, args: dict[str, Any]) -> Decision:
        """Check if a tool call is allowed."""
        # Dangerous command check (always deny)
        if side_effect == SideEffect.EXECUTE:
            command = args.get("command", "")
            if self._is_dangerous(command):
                return Decision.DENY

        # Session override
        if tool_name in self._session_overrides:
            return self._session_overrides[tool_name]

        # Auto-approve mode
        if self._auto_approve:
            return Decision.ALLOW

        # Default policy by side effect
        if side_effect == SideEffect.READ:
            return Decision.ALLOW
        return Decision.ASK

    def set_session_override(self, tool_name: str, decision: Decision) -> None:
        """Set a session-level override for a tool."""
        self._session_overrides[tool_name] = decision

    def _is_dangerous(self, command: str) -> bool:
        return any(p.search(command) for p in DANGEROUS_PATTERNS)

    def format_prompt(self, tool_name: str, args: dict[str, Any]) -> str:
        """Format an interactive permission prompt."""
        if tool_name == "bash":
            detail = args.get("command", "")
        elif tool_name in ("write_file", "str_replace"):
            detail = args.get("path", "")
        else:
            detail = str(args)[:100]
        return f"🔧 {tool_name}: {detail}\n[Y]es  [N]o  [A]lways allow {tool_name}  [D]eny always  > "

    def prompt_user(self, tool_name: str, side_effect: SideEffect, args: dict[str, Any]) -> Decision:
        """Interactive permission prompt. Returns Decision and sets session override if requested."""
        prompt = self.format_prompt(tool_name, args)
        try:
            choice = input(prompt).strip().lower()
        except (EOFError, KeyboardInterrupt):
            return Decision.DENY
        if choice in ("y", "yes", ""):
            return Decision.ALLOW
        if choice in ("a", "always"):
            self.set_session_override(tool_name, Decision.ALLOW)
            return Decision.ALLOW
        if choice in ("d", "deny"):
            self.set_session_override(tool_name, Decision.DENY)
            return Decision.DENY
        return Decision.DENY
