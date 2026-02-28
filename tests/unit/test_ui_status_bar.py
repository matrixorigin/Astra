"""Tests for adaptive status bar."""

from cli.ui.status_bar import StatusBar, _format_tokens


class TestFormatTokens:
    def test_zero(self):
        assert _format_tokens(0) == "0"

    def test_small(self):
        assert _format_tokens(999) == "999"

    def test_thousands(self):
        assert _format_tokens(1500) == "1.5k"

    def test_exact_thousand(self):
        assert _format_tokens(1000) == "1.0k"

    def test_millions(self):
        assert _format_tokens(1_000_000) == "1.0M"

    def test_large(self):
        assert _format_tokens(2_500_000) == "2.5M"


class TestStatusBar:
    def test_default_compact(self):
        sb = StatusBar()
        assert sb.toolbar() is None

    def test_verbose_shows_info(self):
        sb = StatusBar()
        sb.verbose = True
        sb.update(session_id="ses_abc123", model="gpt-4", turn=3, tokens_used=1500)
        result = sb.toolbar()
        assert result is not None
        assert "ses_abc12345" not in result  # truncated to 12 chars
        assert "ses_abc123" in result
        assert "gpt-4" in result
        assert "turn:3" in result
        assert "1.5k" in result

    def test_update_partial(self):
        sb = StatusBar()
        sb.verbose = True
        sb.update(session_id="s1")
        sb.update(model="claude")
        result = sb.toolbar()
        assert "s1" in result
        assert "claude" in result

    def test_default_values(self):
        sb = StatusBar()
        sb.verbose = True
        result = sb.toolbar()
        assert "—" in result  # no session
        assert "(default)" in result  # no model
        assert "turn:0" in result
        assert "tokens:0" in result

    def test_toggle(self):
        sb = StatusBar()
        sb.verbose = True
        assert sb.toolbar() is not None
        sb.verbose = False
        assert sb.toolbar() is None
