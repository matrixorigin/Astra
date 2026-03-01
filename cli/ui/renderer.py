"""Rich renderer — drop-in replacement for StderrRenderer.

Implements the Renderer Protocol from edge_chat_loop.py using rich for
streaming markdown, tool spinners, and styled error/info output.
"""

import sys
import time
from typing import Any

from rich.console import Console
from rich.panel import Panel

from cli.ui.markdown import StreamingMarkdown
from cli.ui.theme import THEME


class RichRenderer:
    """Terminal renderer using rich for styled output."""

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console(stderr=True)
        self._md: StreamingMarkdown | None = None
        self._spinner: Any = None
        self._t0: float = 0

    def begin_response(self) -> None:
        """Start thinking indicator. Markdown Live starts on first text chunk."""
        self._t0 = time.monotonic()
        self._md = StreamingMarkdown(console=self._console)
        self._md.start()
        self._md.show_thinking()
        self._spinner = True  # flag for _stop_spinner

    def _stop_spinner(self) -> None:
        if self._spinner and self._md is not None:
            self._md._hide_thinking()
        self._spinner = None

    def end_response(self) -> str:
        """Stop streaming and return accumulated text."""
        self._stop_spinner()
        if self._md is not None:
            text = self._md.finish()
            self._md = None
            return text
        return ""

    def text(self, chunk: str) -> None:
        self._stop_spinner()
        if self._md is None:
            self._md = StreamingMarkdown(console=self._console)
        if not self._md._live:
            self._md.start()
        self._md.feed(chunk)

    def thinking(self, label: str = "Thinking…") -> None:
        """Show a thinking indicator during long pauses in LLM output."""
        if self._md is not None:
            self._md.show_thinking(label)

    def thinking_hide(self) -> None:
        """Explicitly hide the thinking indicator."""
        if self._md is not None:
            self._md._hide_thinking()

    def tool_start(self, name: str, args: dict[str, Any]) -> None:
        self._stop_spinner()
        if self._md is not None:
            self.end_response()
        detail = args.get("command", args.get("path", ""))
        self._console.print(
            f"  {THEME.tool} [yellow]{name}[/yellow]: {detail}… ",
            end="", highlight=False,
        )

    def tool_done(self, name: str, result: str, error: bool) -> None:
        if error:
            self._console.print(f"[red]{THEME.failure}[/red]")
            if result:
                self._console.print(f"    [dim red]{result}[/dim red]")
        else:
            self._console.print(f"[green]{THEME.success}[/green]")

    def error(self, msg: str) -> None:
        self._stop_spinner()
        if self._md is not None:
            self.end_response()
        self._console.print(Panel(msg, border_style="red", title="Error", title_align="left"))

    def info(self, msg: str) -> None:
        self._console.print(msg, style=THEME.info_style)

    def stats(self, usage: dict[str, int]) -> None:
        """Display token usage and elapsed time after a response."""
        parts = []
        if self._t0:
            elapsed = time.monotonic() - self._t0
            parts.append(f"{elapsed:.1f}s")
        if "prompt_tokens" in usage:
            parts.append(f"in:{usage['prompt_tokens']}")
        if "completion_tokens" in usage:
            parts.append(f"out:{usage['completion_tokens']}")
        if "total_tokens" in usage:
            parts.append(f"total:{usage['total_tokens']}")
        if parts:
            self._console.print()
            self._console.print(f"  [dim]{' · '.join(parts)}[/dim]")


class SimpleRenderer:
    """Plain text renderer for non-TTY / piped output."""

    def text(self, chunk: str) -> None:
        sys.stdout.write(chunk)
        sys.stdout.flush()

    def tool_start(self, name: str, args: dict[str, Any]) -> None:
        detail = args.get("command", args.get("path", ""))
        sys.stderr.write(f"  {name}: {detail}… ")
        sys.stderr.flush()

    def tool_done(self, name: str, result: str, error: bool) -> None:
        sys.stderr.write("FAIL\n" if error else "OK\n")
        sys.stderr.flush()

    def error(self, msg: str) -> None:
        sys.stderr.write(f"ERROR: {msg}\n")
        sys.stderr.flush()

    def info(self, msg: str) -> None:
        sys.stderr.write(f"{msg}\n")
        sys.stderr.flush()
