"""Tests for CLI UI theme."""

from rich.style import Style

from cli.ui.theme import THEME, Theme


class TestTheme:
    def test_instantiates(self):
        t = Theme()
        assert isinstance(t, Theme)

    def test_singleton_theme(self):
        assert isinstance(THEME, Theme)

    def test_icons_non_empty(self):
        for name in ("prompt", "assistant", "tool", "success", "failure", "spinner", "info_icon"):
            val = getattr(THEME, name)
            assert isinstance(val, str) and len(val) > 0, f"{name} should be non-empty str"

    def test_styles_are_rich_style(self):
        for name in (
            "assistant_style",
            "tool_style",
            "success_style",
            "failure_style",
            "error_style",
            "info_style",
            "prompt_style",
            "dim_style",
            "border_style",
        ):
            val = getattr(THEME, name)
            assert isinstance(val, Style), f"{name} should be rich.style.Style"

    def test_frozen(self):
        import pytest

        with pytest.raises(AttributeError):
            THEME.prompt = ">"  # type: ignore[misc]
