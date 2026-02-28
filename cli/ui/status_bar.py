"""Adaptive status bar — bottom toolbar for prompt_toolkit."""


def _format_tokens(n: int) -> str:
    """Format token count: 0→'0', 1500→'1.5k', 1000000→'1.0M'."""
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


class StatusBar:
    """Adaptive bottom toolbar toggled by /verbose and /compact."""

    def __init__(self) -> None:
        self.verbose: bool = False
        self._session_id: str = ""
        self._model: str = ""
        self._turn: int = 0
        self._tokens: int = 0

    def update(
        self,
        session_id: str | None = None,
        model: str | None = None,
        turn: int | None = None,
        tokens_used: int | None = None,
    ) -> None:
        if session_id is not None:
            self._session_id = session_id
        if model is not None:
            self._model = model
        if turn is not None:
            self._turn = turn
        if tokens_used is not None:
            self._tokens = tokens_used

    def toolbar(self) -> str | None:
        """Return toolbar text for prompt_toolkit, or None if compact mode."""
        if not self.verbose:
            return None
        sid = self._session_id[:12] if self._session_id else "—"
        model = self._model or "(default)"
        return (
            f" session:{sid} │ model:{model} │ "
            f"turn:{self._turn} │ tokens:{_format_tokens(self._tokens)}"
        )
