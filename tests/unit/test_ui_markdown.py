"""Tests for streaming markdown accumulator."""

from io import StringIO

from rich.console import Console

from cli.ui.markdown import StreamingMarkdown, install_prettier_code_blocks


def _make_console() -> tuple[Console, StringIO]:
    buf = StringIO()
    return Console(file=buf, force_terminal=True, width=80), buf


class TestStreamingMarkdown:
    def test_feed_accumulates(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.feed("Hello ")
        sm.feed("world")
        assert sm.text == "Hello world"

    def test_finish_returns_full_text(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.feed("abc")
        sm.feed("def")
        result = sm.finish()
        assert result == "abcdef"

    def test_feed_empty_string(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.feed("")
        sm.feed("")
        assert sm.text == ""

    def test_partial_code_fence_no_crash(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm.feed("```python\nprint('hi')")  # no closing fence
        sm.finish()  # should not crash
        assert "print('hi')" in sm.text

    def test_complete_code_block(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm.feed("```python\nprint('hello')\n```\n")
        result = sm.finish()
        assert "print('hello')" in result

    def test_live_start_stop(self):
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.start()
        assert sm._live is not None
        sm.feed("test")
        sm.finish()
        assert sm._live is None

    def test_finish_without_start(self):
        """finish() without start() should not crash."""
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.feed("no live")
        result = sm.finish()
        assert result == "no live"


class TestCodeBlockInstall:
    def test_install_idempotent(self):
        """Calling install_prettier_code_blocks multiple times is safe."""
        install_prettier_code_blocks()
        install_prettier_code_blocks()
        from rich.markdown import Markdown
        assert Markdown.elements["fence"].__name__ == "SimpleCodeBlock"
