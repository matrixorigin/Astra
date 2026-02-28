"""Streaming markdown — accumulates chunks and re-renders via rich.Live."""

from rich.console import Console, ConsoleOptions, RenderResult
from rich.live import Live
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
    """Accumulates text chunks and re-renders markdown via rich.Live."""

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console()
        self._buffer = ""
        self._live: Live | None = None

    def start(self) -> None:
        self._live = Live("", console=self._console, vertical_overflow="visible")
        self._live.start()

    def feed(self, chunk: str) -> None:
        self._buffer += chunk
        if self._live is not None:
            try:
                self._live.update(Markdown(self._buffer))
            except Exception:
                # Partial markdown (e.g. incomplete fence) — render as-is
                self._live.update(Text(self._buffer))

    def finish(self) -> str:
        """Stop live rendering and return accumulated text."""
        if self._live is not None:
            try:
                self._live.update(Markdown(self._buffer))
            except Exception:
                self._live.update(Text(self._buffer))
            self._live.stop()
            self._live = None
        return self._buffer

    @property
    def text(self) -> str:
        return self._buffer
