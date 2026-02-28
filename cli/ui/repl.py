"""Interactive REPL — prompt_toolkit session with autocomplete and history."""

from pathlib import Path
from typing import Any

from prompt_toolkit import PromptSession
from prompt_toolkit.completion import Completer, Completion
from prompt_toolkit.document import Document
from prompt_toolkit.history import FileHistory

SLASH_COMMANDS: list[tuple[str, str]] = [
    ("/help", "Show available commands"),
    ("/model", "List or select model"),
    ("/session", "Show current session info"),
    ("/clear", "Start a new session"),
    ("/login", "Login to API"),
    ("/logout", "Logout"),
    ("/verbose", "Show status bar"),
    ("/compact", "Hide status bar"),
    ("/history", "Show recent turns"),
    ("/copy", "Copy last response to clipboard"),
    ("/doctor", "Run diagnostics"),
    ("/version", "Show version info"),
    ("/exit", "Exit chat"),
    ("/quit", "Exit chat"),
]


class SlashCommandCompleter(Completer):
    """Autocomplete slash commands."""

    def get_completions(self, document: Document, complete_event: Any) -> Any:
        text = document.text_before_cursor.lstrip()
        if not text.startswith("/"):
            return
        for cmd, desc in SLASH_COMMANDS:
            if cmd.startswith(text):
                yield Completion(cmd, start_position=-len(text), display_meta=desc)


def create_session(
    history_path: Path | None = None,
    bottom_toolbar: Any = None,
) -> PromptSession:
    """Create a prompt_toolkit session with autocomplete and history."""
    hp = history_path or Path.home() / ".mo-agent" / "history"
    hp.parent.mkdir(parents=True, exist_ok=True)
    return PromptSession(
        completer=SlashCommandCompleter(),
        history=FileHistory(str(hp)),
        bottom_toolbar=bottom_toolbar,
        multiline=False,  # Enter submits; Alt+Enter for newline via default bindings
    )


class InputResult:
    """Result of get_input — either text, EOF, or interrupt."""

    def __init__(self, text: str = "", eof: bool = False, interrupted: bool = False):
        self.text = text
        self.eof = eof
        self.interrupted = interrupted


def get_input(session: PromptSession, prompt_text: str = "❯ ") -> InputResult:
    """Read input, handling Ctrl+D and Ctrl+C gracefully."""
    try:
        text = session.prompt(prompt_text)
        return InputResult(text=text)
    except EOFError:
        return InputResult(eof=True)
    except KeyboardInterrupt:
        return InputResult(interrupted=True)
