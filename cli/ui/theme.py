"""Theme constants — colors, styles, icons for the CLI UI."""

from dataclasses import dataclass, field

from rich.style import Style


@dataclass(frozen=True)
class Theme:
    """Immutable theme configuration."""

    # Icons
    prompt: str = "❯"
    assistant: str = "✦"
    tool: str = "⚡"
    success: str = "✓"
    failure: str = "✗"
    spinner: str = "●"
    info_icon: str = "ℹ"
    session_icon: str = "📝"
    model_icon: str = "🤖"

    # Styles
    assistant_style: Style = field(default_factory=lambda: Style(color="cyan", bold=True))
    tool_style: Style = field(default_factory=lambda: Style(color="yellow"))
    success_style: Style = field(default_factory=lambda: Style(color="green"))
    failure_style: Style = field(default_factory=lambda: Style(color="red"))
    error_style: Style = field(default_factory=lambda: Style(color="red", bold=True))
    info_style: Style = field(default_factory=lambda: Style(dim=True))
    prompt_style: Style = field(default_factory=lambda: Style(color="bright_cyan", bold=True))
    dim_style: Style = field(default_factory=lambda: Style(dim=True))
    border_style: Style = field(default_factory=lambda: Style(color="bright_black"))


THEME = Theme()
