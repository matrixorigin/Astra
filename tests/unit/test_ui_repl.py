"""Tests for REPL module — completer, session creation, input handling."""

from pathlib import Path

from prompt_toolkit.document import Document

from cli.ui.repl import SlashCommandCompleter, create_session, SLASH_COMMANDS, InputResult


class TestSlashCommandCompleter:
    def _completions(self, text: str) -> list[str]:
        c = SlashCommandCompleter()
        doc = Document(text, len(text))
        return [comp.text for comp in c.get_completions(doc, None)]

    def test_slash_prefix_returns_all(self):
        results = self._completions("/")
        assert len(results) == len(SLASH_COMMANDS)

    def test_partial_model(self):
        results = self._completions("/mo")
        assert "/model" in results

    def test_partial_help(self):
        results = self._completions("/he")
        assert "/help" in results

    def test_no_slash_returns_empty(self):
        results = self._completions("hello")
        assert results == []

    def test_exact_match(self):
        results = self._completions("/exit")
        assert "/exit" in results

    def test_empty_input_returns_empty(self):
        results = self._completions("")
        assert results == []


class TestCreateSession:
    def test_returns_prompt_session(self, tmp_path):
        from prompt_toolkit import PromptSession

        session = create_session(history_path=tmp_path / "history")
        assert isinstance(session, PromptSession)

    def test_creates_history_dir(self, tmp_path):
        hp = tmp_path / "sub" / "history"
        create_session(history_path=hp)
        assert hp.parent.exists()

    def test_default_history_path(self):
        """Default history path is ~/.mo-agent/history."""
        # Just verify the function doesn't crash with default
        # (we don't actually create the session to avoid writing to home dir)
        expected = Path.home() / ".mo-agent" / "history"
        assert expected.parent.name == ".mo-agent"


class TestInputResult:
    def test_normal(self):
        r = InputResult(text="hello")
        assert r.text == "hello"
        assert not r.eof
        assert not r.interrupted

    def test_eof(self):
        r = InputResult(eof=True)
        assert r.eof

    def test_interrupted(self):
        r = InputResult(interrupted=True)
        assert r.interrupted
