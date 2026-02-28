"""Streaming markdown — streams raw text, renders final markdown on finish."""

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
    """Streams raw text chunks during generation, renders markdown on finish.

    During streaming: writes raw text directly (no Live, no cursor tricks).
    On finish: clears the raw output and prints a single rich Markdown render.
    """

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console()
        self._buffer = ""
        self._live = False  # just a flag now, not a Live object
        self._line_count = 0  # track lines written for cleanup

    def start(self) -> None:
        self._live = True

    def feed(self, chunk: str) -> None:
        self._buffer += chunk
        if self._live:
            # Write raw text directly — no Live re-rendering
            self._console.file.write(chunk)
            self._console.file.flush()
            self._line_count += chunk.count("\n")

    def finish(self) -> str:
        """Stop streaming and return accumulated text."""
        if self._live:
            self._live = False
            # Ensure trailing newline after raw stream
            if self._buffer and not self._buffer.endswith("\n"):
                self._console.file.write("\n")
                self._console.file.flush()
        return self._buffer

    @property
    def text(self) -> str:
        return self._buffer
