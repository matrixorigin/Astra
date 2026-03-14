"""Permission system for edge tool execution.

Default policy:
  READ tools → auto-allow
  WRITE tools → ask user
  EXECUTE tools → ask user
  Dangerous commands → deny
  Suspicious bypass attempts → ask with warning
"""

import re
from enum import Enum
from typing import Any

from cli.tools.base import SideEffect

# Commands that are always denied — truly dangerous, no legitimate use case
DANGEROUS_PATTERNS = [
    re.compile(r"\brm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?/\s*$"),  # rm -rf /
    re.compile(r"\bsudo\b"),
    re.compile(r"\bmkfs\b"),
    re.compile(r"\bdd\s+.*of=/dev/"),
    re.compile(r"\b:(){ :\|:& };:"),  # fork bomb
    re.compile(r"\bchmod\s+777\s+/"),
    re.compile(r"\bcurl\b.*\|\s*(ba)?sh"),  # curl | sh
]

# Commands that MAY be bypassing tool restrictions — require explicit user approval.
# Unlike DANGEROUS_PATTERNS (always deny), these are ASK with a warning because
# users may have legitimate reasons to delete source files.
#
# Design rationale:
# - Agent tried write_file on existing file → rejected ("file already exists")
# - Agent then tries `rm file.py` → this MIGHT be a bypass attempt
# - We don't DENY because user might legitimately want to delete the file
# - We ASK with a warning so user can make an informed decision
# - The prompt constraint "NEVER use bash rm to delete and recreate" guides the agent
TOOL_BYPASS_WARNING_PATTERNS = [
    # rm on source files — possibly bypassing write_file protection
    # Matches: rm file.py, rm -f file.py, rm path/to/file.py
    # Also matches chained: ls && rm file.py, echo x; rm file.py
    # Does NOT match: rm -rf dir/, rm -r dir (directory operations)
    # Does NOT match: echo rm file.py, grep rm file.py (rm as argument)
    re.compile(
        r"(?:^|[;&|]\s*)\s*rm\s+(-[a-zA-Z]*\s+)?[^\s]+\.(py|js|ts|go|rs|java|c|cpp|h|md|txt|json|yaml|yml|toml|sh)\b"
    ),
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
        # Track if current command has a bypass warning
        self._current_bypass_warning: str | None = None

    def check(self, tool_name: str, side_effect: SideEffect, args: dict[str, Any]) -> Decision:
        """Check if a tool call is allowed."""
        self._current_bypass_warning = None  # Reset

        # Dangerous command check (always deny)
        if side_effect == SideEffect.EXECUTE:
            command = args.get("command", "")
            if self._is_dangerous(command):
                return Decision.DENY
            # Tool bypass warning — flag for user attention but don't auto-deny
            # User may have legitimate reasons to delete source files
            if self._is_potential_bypass(command):
                self._current_bypass_warning = (
                    "⚠️  This command deletes a source file. If the agent is doing this "
                    "to bypass write_file's 'file exists' check, consider rejecting."
                )
                # Don't return here — fall through to ASK (not auto-allow)
                # Even with session override, we want user to see the warning
                if tool_name in self._session_overrides:
                    # Override exists, but still ASK for bypass attempts
                    return Decision.ASK
                return Decision.ASK

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

    def get_bypass_warning(self) -> str | None:
        """Get warning message if current command is a potential bypass attempt."""
        return self._current_bypass_warning

    def set_session_override(self, tool_name: str, decision: Decision) -> None:
        """Set a session-level override for a tool."""
        self._session_overrides[tool_name] = decision

    def _is_dangerous(self, command: str) -> bool:
        return any(p.search(command) for p in DANGEROUS_PATTERNS)

    def _is_potential_bypass(self, command: str) -> bool:
        """Check if command might be bypassing tool-level restrictions.

        Returns True for commands like `rm file.py` that could circumvent
        write_file's "file already exists" protection. Unlike _is_dangerous(),
        this returns ASK (not DENY) because users may have legitimate reasons.
        """
        return any(p.search(command) for p in TOOL_BYPASS_WARNING_PATTERNS)

    def format_prompt(self, tool_name: str, args: dict[str, Any]) -> str:
        """Format an interactive permission prompt (rich markup)."""
        if tool_name == "bash":
            detail = args.get("command", "")
        elif tool_name in ("write_file", "str_replace"):
            detail = args.get("path", "")
        else:
            detail = str(args)[:100]
        return f"⚡ [bold]{tool_name}[/bold]: [dim]{detail}[/dim]"

    def format_prompt_plain(self, tool_name: str, args: dict[str, Any]) -> str:
        """Format prompt as plain text (non-TTY fallback)."""
        if tool_name == "bash":
            detail = args.get("command", "")
        elif tool_name in ("write_file", "str_replace"):
            detail = args.get("path", "")
        else:
            detail = str(args)[:100]
        return f"⚡ {tool_name}: {detail}"

    def prompt_user(
        self, tool_name: str, side_effect: SideEffect, args: dict[str, Any]
    ) -> Decision:
        """Interactive permission prompt with rich formatting when available.

        Raises KeyboardInterrupt on Ctrl-C so the caller can cancel the
        entire turn instead of silently treating it as "deny".
        """
        import sys

        if sys.stdin.isatty():
            try:
                from rich.console import Console
                from rich.panel import Panel

                console = Console(stderr=True)
                console.print(
                    Panel(
                        self.format_prompt(tool_name, args),
                        border_style="yellow",
                        title="Permission",
                        title_align="left",
                    )
                )
                console.print(
                    "  [green]\\[Y]es[/green]  [red]\\[N]o[/red]  "
                    "[cyan]\\[A]lways[/cyan]  [red]\\[D]eny always[/red]",
                )
                choice = console.input("[dim]  >[/dim] ").strip().lower()
            except EOFError:
                return Decision.DENY
            except KeyboardInterrupt:
                raise
        else:
            prompt = self.format_prompt_plain(tool_name, args)
            try:
                choice = input(f"{prompt}\n[Y/n/a/d] > ").strip().lower()
            except EOFError:
                return Decision.DENY
            except KeyboardInterrupt:
                raise

        if choice in ("y", "yes", ""):
            return Decision.ALLOW
        if choice in ("a", "always"):
            self.set_session_override(tool_name, Decision.ALLOW)
            return Decision.ALLOW
        if choice in ("d", "deny"):
            self.set_session_override(tool_name, Decision.DENY)
            return Decision.DENY
        return Decision.DENY
