"""Test CLI rendering to prevent extra blank lines.

Regression: tool execution added extra blank lines before tool output
because end_response() was injecting newlines in the rerender=False path.
"""

from io import StringIO

from rich.console import Console

from cli.ui.renderer import RichRenderer
from cli.ui.markdown import StreamingMarkdown


def test_tool_start_no_triple_newlines():
    """text → tool_start must not produce triple newlines."""
    output = StringIO()
    console = Console(file=output, force_terminal=True, width=80)
    renderer = RichRenderer(console=console)

    renderer.begin_response()
    renderer.text("Some response text")
    renderer.tool_start("test_tool", {"command": "ls"})

    assert "\n\n\n" not in output.getvalue()


def test_markdown_finish_no_rerender_minimal_trailing():
    """finish(rerender=False) should leave at most 1 trailing newline."""
    output = StringIO()
    console = Console(file=output, force_terminal=True, width=80)
    md = StreamingMarkdown(console=console)

    md.start()
    md.feed("Some text")
    md.finish(rerender=False)

    trailing = len(output.getvalue()) - len(output.getvalue().rstrip("\n"))
    assert trailing <= 1


def test_tool_start_clears_markdown_state():
    """After tool_start, the renderer's markdown instance must be None."""
    output = StringIO()
    console = Console(file=output, force_terminal=True, width=80)
    renderer = RichRenderer(console=console)

    renderer.begin_response()
    renderer.text("text")
    renderer.tool_start("t", {"command": "x"})

    assert renderer._md is None


def test_multiple_tools_no_excessive_blanks():
    """Three consecutive tool calls should not produce >2 consecutive blank lines."""
    output = StringIO()
    console = Console(file=output, force_terminal=True, width=80)
    renderer = RichRenderer(console=console)

    renderer.begin_response()
    renderer.text("Running:\n\n")
    for i in range(3):
        renderer.tool_start(f"t{i}", {"command": f"c{i}"})
        renderer.tool_done(f"t{i}", "", error=False)

    max_consecutive = 0
    cur = 0
    for line in output.getvalue().split("\n"):
        if not line.strip():
            cur += 1
            max_consecutive = max(max_consecutive, cur)
        else:
            cur = 0
    assert max_consecutive <= 2
