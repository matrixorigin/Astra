"""Streaming markdown — streams raw text, renders rich Markdown on finish."""

from rich.cells import cell_len
from rich.console import Console, ConsoleOptions, RenderResult
from rich.markdown import CodeBlock, Markdown
from rich.syntax import Syntax
from rich.text import Text


def install_prettier_code_blocks() -> None:
    """Replace default code block renderer with syntax-highlighted version."""

    class SimpleCodeBlock(CodeBlock):
        def __rich_console__(self, console: Console, options: ConsoleOptions) -> RenderResult:
            code = str(self.text).rstrip()
            yield Text(self.lexer_name, style="dim")
            yield Syntax(
                code, self.lexer_name, theme=self.theme,
                background_color="default", word_wrap=True,
            )
            yield Text(f"/{self.lexer_name}", style="dim")

    Markdown.elements["fence"] = SimpleCodeBlock


# Install once at import time
install_prettier_code_blocks()


class StreamingMarkdown:
    """Streams raw text during generation, renders rich Markdown on finish.

    During streaming: writes raw text directly (no Live, no duplication).
    On finish (terminal): erases raw lines, prints rich Markdown render.
    On finish (non-terminal): just ensures trailing newline.
    """

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console()
        self._buffer = ""
        self._live = False
        self._raw_lines = 0  # terminal lines occupied (for erase)
        # Track the display-width of the current (unterminated) line so that
        # chunks that arrive without a trailing newline are handled correctly
        # when the *next* chunk continues the same visual line.
        self._current_line_width = 0
        self._thinking_shown = False

    def start(self) -> None:
        self._live = True

    def _count_lines(self, text: str) -> int:
        """Count terminal lines occupied by *text*, accounting for CJK width and wrapping.

        Uses rich.cells.cell_len for accurate display-width (handles CJK,
        emoji, zero-width chars).  Tracks _current_line_width across calls
        so partial lines split across chunks are counted correctly.

        Wrapping model: terminals auto-wrap when cursor reaches column
        ``width`` (i.e. after writing ``width`` columns on a line).
        """
        width = self._console.width or 80
        lines = 0
        for ch in text:
            if ch == "\n":
                lines += 1
                self._current_line_width = 0
            else:
                cw = cell_len(ch)
                # Terminals cannot split a wide (CJK/emoji) character across
                # the right margin.  If a 2-cell char would start at the last
                # column, the terminal pads that column and wraps the char to
                # the next line.
                if cw == 2 and self._current_line_width == width - 1:
                    lines += 1
                    self._current_line_width = cw
                else:
                    self._current_line_width += cw
                    if self._current_line_width >= width:
                        lines += 1
                        self._current_line_width = self._current_line_width - width
        return lines

    def show_thinking(self) -> None:
        """Display a transient thinking indicator below the current text."""
        if not self._live or not self._console.is_terminal or self._thinking_shown:
            return
        f = self._console.file
        # Save cursor position so _hide_thinking can restore exactly,
        # even if the newline triggers a terminal scroll.
        f.write("\033[s\n\033[2m  ⏳ Thinking…\033[0m")
        f.flush()
        self._thinking_shown = True

    def _hide_thinking(self) -> None:
        if not self._thinking_shown:
            return
        f = self._console.file
        # Erase the thinking line, then restore saved cursor position.
        f.write("\r\033[2K\033[u")
        f.flush()
        self._thinking_shown = False

    def feed(self, chunk: str) -> None:
        self._hide_thinking()
        self._buffer += chunk
        if self._live:
            f = self._console.file
            f.write(chunk)
            f.flush()
            self._raw_lines += self._count_lines(chunk)

    def finish(self) -> str:
        """Stop streaming. On terminals, replace raw text with rendered markdown."""
        self._hide_thinking()
        if self._live:
            self._live = False
            f = self._console.file
            if self._console.is_terminal and self._buffer:
                # Erase raw output: clear the current (last) line first,
                # then move up through each wrapped line and clear it.
                f.write("\r\033[2K")
                for _ in range(self._raw_lines):
                    f.write("\033[A\033[2K")
                f.flush()
                # Render final markdown
                self._console.print(Markdown(self._buffer))
            else:
                if self._buffer and not self._buffer.endswith("\n"):
                    f.write("\n")
                    f.flush()
        return self._buffer

    @property
    def text(self) -> str:
        return self._buffer
