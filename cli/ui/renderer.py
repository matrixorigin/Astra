"""Rich renderer — drop-in replacement for StderrRenderer.

Implements the Renderer Protocol from edge_chat_loop.py using rich for
streaming markdown, tool spinners, and styled error/info output.
"""

import sys
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

    def begin_response(self) -> None:
        """Start streaming markdown context."""
        self._md = StreamingMarkdown(console=self._console)
        self._md.start()

    def end_response(self) -> str:
        """Stop streaming and return accumulated text."""
        if self._md is not None:
            text = self._md.finish()
            self._md = None
            return text
        return ""

    def text(self, chunk: str) -> None:
        if self._md is None:
            self.begin_response()
        assert self._md is not None
        self._md.feed(chunk)

    def tool_start(self, name: str, args: dict[str, Any]) -> None:
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
        else:
            self._console.print(f"[green]{THEME.success}[/green]")

    def error(self, msg: str) -> None:
        if self._md is not None:
            self.end_response()
        self._console.print(Panel(msg, border_style="red", title="Error", title_align="left"))

    def info(self, msg: str) -> None:
        self._console.print(msg, style=THEME.info_style)


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
