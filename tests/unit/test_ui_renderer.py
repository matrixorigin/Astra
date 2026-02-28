"""Tests for RichRenderer and SimpleRenderer."""

from io import StringIO
from typing import Any

from rich.console import Console

from cli.ui.renderer import RichRenderer, SimpleRenderer


def _make_console() -> tuple[Console, StringIO]:
    buf = StringIO()
    return Console(file=buf, force_terminal=True, width=80), buf


class TestRichRendererProtocol:
    """Verify RichRenderer satisfies the Renderer Protocol structurally."""

    def test_has_text(self):
        assert callable(getattr(RichRenderer, "text", None))

    def test_has_tool_start(self):
        assert callable(getattr(RichRenderer, "tool_start", None))

    def test_has_tool_done(self):
        assert callable(getattr(RichRenderer, "tool_done", None))

    def test_has_error(self):
        assert callable(getattr(RichRenderer, "error", None))

    def test_has_info(self):
        assert callable(getattr(RichRenderer, "info", None))


class TestSimpleRendererProtocol:
    def test_has_all_methods(self):
        for method in ("text", "tool_start", "tool_done", "error", "info"):
            assert callable(getattr(SimpleRenderer, method, None))


class TestRichRendererOutput:
    def test_text_accumulates(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.begin_response()
        r.text("Hello ")
        r.text("world")
        text = r.end_response()
        assert text == "Hello world"

    def test_text_auto_begins_response(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.text("auto start")
        text = r.end_response()
        assert text == "auto start"

    def test_tool_start_contains_name(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.tool_start("read_file", {"path": "main.py"})
        output = buf.getvalue()
        assert "read_file" in output

    def test_tool_done_success(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.tool_done("read_file", "content", error=False)
        output = buf.getvalue()
        assert "✓" in output

    def test_tool_done_error(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.tool_done("bash", "failed", error=True)
        output = buf.getvalue()
        assert "✗" in output

    def test_tool_done_error_shows_detail(self):
        """Regression: tool_done must display the error message, not just ✗.

        Before the fix, error details were silently discarded — users saw
        only "✗" with no indication of what went wrong.
        """
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.tool_done("write_file", "Missing required: path", error=True)
        output = buf.getvalue()
        assert "✗" in output
        assert "path" in output

    def test_tool_done_error_empty_result_no_extra_line(self):
        """Empty error result should not print a blank detail line."""
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.tool_done("bash", "", error=True)
        output = buf.getvalue()
        assert "✗" in output
        # Should only have the ✗ line, no extra blank detail line
        lines = [l for l in output.split("\n") if l.strip()]
        assert len(lines) == 1

    def test_error_contains_message(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.error("something broke")
        output = buf.getvalue()
        assert "something broke" in output

    def test_info_produces_output(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.info("retrying...")
        output = buf.getvalue()
        assert "retrying" in output

    def test_tool_start_ends_streaming(self):
        """tool_start should end any active streaming markdown."""
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.begin_response()
        r.text("some text")
        r.tool_start("bash", {"command": "ls"})
        # _md should be None after tool_start ends it
        assert r._md is None

    def test_error_ends_streaming(self):
        console, buf = _make_console()
        r = RichRenderer(console=console)
        r.begin_response()
        r.text("partial")
        r.error("oops")
        assert r._md is None


class TestSimpleRendererOutput:
    def test_text(self, capsys):
        r = SimpleRenderer()
        r.text("hello")
        assert capsys.readouterr().out == "hello"

    def test_tool_start(self, capsys):
        r = SimpleRenderer()
        r.tool_start("bash", {"command": "ls"})
        assert "bash" in capsys.readouterr().err

    def test_tool_done_ok(self, capsys):
        r = SimpleRenderer()
        r.tool_done("bash", "ok", error=False)
        assert "OK" in capsys.readouterr().err

    def test_tool_done_fail(self, capsys):
        r = SimpleRenderer()
        r.tool_done("bash", "err", error=True)
        assert "FAIL" in capsys.readouterr().err

    def test_error(self, capsys):
        r = SimpleRenderer()
        r.error("bad")
        assert "bad" in capsys.readouterr().err

    def test_info(self, capsys):
        r = SimpleRenderer()
        r.info("note")
        assert "note" in capsys.readouterr().err
