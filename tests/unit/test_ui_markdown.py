"""Tests for streaming markdown accumulator."""

from io import StringIO

from rich.console import Console

from cli.ui.markdown import StreamingMarkdown, install_prettier_code_blocks


def _make_console(width: int = 80) -> tuple[Console, StringIO]:
    buf = StringIO()
    return Console(file=buf, force_terminal=True, width=width), buf


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
        assert sm._live
        sm.feed("test")
        sm.finish()
        assert not sm._live

    def test_finish_without_start(self):
        """finish() without start() should not crash."""
        console, _ = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.feed("no live")
        result = sm.finish()
        assert result == "no live"

    def test_non_terminal_no_erase(self):
        """Non-terminal output: no ANSI erase, just raw text."""
        buf = StringIO()
        console = Console(file=buf, force_terminal=False, width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm.feed("Hello\n")
        sm.finish()
        output = buf.getvalue()
        assert "\033[A" not in output
        assert "Hello" in output

    def test_terminal_emits_erase_sequences(self):
        """Terminal mode: finish() emits ANSI erase before markdown render."""
        console, buf = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm.feed("Hello\n")
        sm.finish()
        output = buf.getvalue()
        assert "\033[A\033[2K" in output

    def test_terminal_renders_markdown_after_erase(self):
        """Terminal mode: finish() renders markdown (bold becomes styled)."""
        console, buf = _make_console()
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm.feed("**bold text**\n")
        sm.finish()
        output = buf.getvalue()
        # raw "**bold text**" was written first, then erased, then
        # rich.Markdown renders it — the styled output won't have **
        # The raw part has **, the rendered part doesn't
        parts = output.split("\033[2K")
        rendered_part = parts[-1] if parts else output
        assert "**" not in rendered_part
        assert "bold text" in rendered_part


class TestLineCount:
    """Test _count_lines directly — the critical logic for correct erase."""

    def test_simple_lines(self):
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        assert sm._count_lines("hello\n") == 1
        assert sm._raw_lines == 0  # _count_lines doesn't modify _raw_lines
        # but _current_line_width is updated
        assert sm._current_line_width == 0

    def test_no_newline(self):
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("hello")
        assert lines == 0  # no completed line
        assert sm._current_line_width == 5

    def test_wrapping_ascii(self):
        """80 chars at width=80 → fills line exactly, terminal wraps cursor."""
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("A" * 80 + "\n")
        # 80 chars fills width exactly → terminal wraps (1 line).
        # Then \n adds another line. Total = 2.
        assert lines == 2
        assert sm._current_line_width == 0

    def test_wrapping_ascii_exact(self):
        """10 chars at width 10 → wraps, then newline."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("A" * 10 + "\n")
        # 10 chars at width 10: wrap (1 line) + \n (1 line) = 2
        assert lines == 2
        assert sm._current_line_width == 0

    def test_wrapping_ascii_under(self):
        """9 chars at width 10 → no wrap."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("A" * 9 + "\n")
        assert lines == 1

    def test_wrapping_ascii_overflow(self):
        """11 chars at width 10 → 1 wrap + newline = 2 lines."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("A" * 11 + "\n")
        # 10 chars: wrap (1 line), cursor at col 0. 11th char: col 1.
        # \n: 1 line. Total = 2.
        assert lines == 2
        assert sm._current_line_width == 0

    def test_cjk_width(self):
        """CJK chars are 2 columns wide. 40 CJK at width=80 → wraps (80 >= 80)."""
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("你" * 40 + "\n")
        # 40 * 2 = 80 columns = width → wrap (1 line) + \n (1 line) = 2
        assert lines == 2

    def test_cjk_no_wrap(self):
        """39 CJK at width=80 → 78 columns, no wrap."""
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("你" * 39 + "\n")
        assert lines == 1

    def test_cjk_wrapping(self):
        """41 CJK chars at width=80 → 82 columns → 1 wrap + newline."""
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("中" * 41 + "\n")
        # 40 chars = 80 cols → wrap. 41st char: col 2. \n: 1 line. Total = 2.
        assert lines == 2

    def test_cross_chunk_continuity(self):
        """Partial line from chunk 1 continues in chunk 2."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        # Chunk 1: "AAAAA" (5 chars, no newline)
        lines1 = sm._count_lines("AAAAA")
        assert lines1 == 0
        assert sm._current_line_width == 5
        # Chunk 2: "BBBBB\n" (5 more = 10 total → wrap + newline = 2)
        lines2 = sm._count_lines("BBBBB\n")
        assert lines2 == 2
        assert sm._current_line_width == 0

    def test_cross_chunk_no_wrap(self):
        """Partial line from chunk 1 + chunk 2 under width → no wrap."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm._count_lines("AAAA")  # 4 chars
        lines = sm._count_lines("BBB\n")  # 3 more = 7 total, under 10
        assert lines == 1  # just the newline

    def test_cross_chunk_wrapping(self):
        """Partial line from chunk 1 + chunk 2 exceeds width → wrap."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        sm._count_lines("A" * 8)  # 8 chars
        assert sm._current_line_width == 8
        lines = sm._count_lines("B" * 5 + "\n")
        # 8 + 2 more B's = 10 → wrap (1 line). Remaining 3 B's: col 3.
        # \n: 1 line. Total = 2.
        assert lines == 2

    def test_multiple_newlines(self):
        """Empty lines count correctly."""
        console, _ = _make_console(width=80)
        sm = StreamingMarkdown(console=console)
        sm.start()
        lines = sm._count_lines("\n\n\n")
        assert lines == 3

    def test_mixed_cjk_ascii(self):
        """Mixed CJK and ASCII width calculation."""
        console, _ = _make_console(width=10)
        sm = StreamingMarkdown(console=console)
        sm.start()
        # "AB你好C" = 2 + 2 + 2 + 1 = 7 columns (A=1, B=1, 你=2, 好=2, C=1)
        lines = sm._count_lines("AB你好C\n")
        assert lines == 1
        assert sm._current_line_width == 0


class TestCodeBlockInstall:
    def test_install_idempotent(self):
        """Calling install_prettier_code_blocks multiple times is safe."""
        install_prettier_code_blocks()
        install_prettier_code_blocks()
        from rich.markdown import Markdown
        assert Markdown.elements["fence"].__name__ == "SimpleCodeBlock"
