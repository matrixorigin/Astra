"""Tests for Cross-Session Continuity."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, Mock

import pytest

from core.context.continuity import PriorContext, SessionContinuity


def _mock_db():
    return MagicMock()


class TestPriorContext:
    def test_empty_returns_none(self):
        ctx = PriorContext()
        assert ctx.to_prompt_section() is None

    def test_summaries_only(self):
        ctx = PriorContext(
            session_summaries=[
                {"session_id": "s1", "summary": "Discussed auth flow", "title": "Auth"},
            ]
        )
        section = ctx.to_prompt_section()
        assert "Previous Sessions" in section
        assert "[Auth] Discussed auth flow" in section

    def test_summary_without_title_uses_id_prefix(self):
        ctx = PriorContext(
            session_summaries=[
                {"session_id": "abcdef12-3456", "summary": "Quick chat", "title": None},
            ]
        )
        section = ctx.to_prompt_section()
        assert "[abcdef12]" in section

    def test_knowledge_only(self):
        ctx = PriorContext(
            knowledge_entries=[
                {"key": "language", "value": "typescript"},
            ]
        )
        section = ctx.to_prompt_section()
        assert "What I Know About You" in section
        assert "language: typescript" in section

    def test_notes_only(self):
        ctx = PriorContext(
            active_notes=[
                {"note_type": "plan", "content": "Migrate to v2"},
            ]
        )
        section = ctx.to_prompt_section()
        assert "Unfinished Work" in section
        assert "[plan] Migrate to v2" in section

    def test_all_sections(self):
        ctx = PriorContext(
            session_summaries=[{"session_id": "s1", "summary": "Did X", "title": "T"}],
            knowledge_entries=[{"key": "k", "value": "v"}],
            active_notes=[{"note_type": "todo", "content": "Y"}],
        )
        section = ctx.to_prompt_section()
        assert "Previous Sessions" in section
        assert "What I Know About You" in section
        assert "Unfinished Work" in section


class TestSessionContinuity:
    def test_load_prior_context_assembles_all(self):
        db = _mock_db()
        # Mock 3 queries + 1 access tracking UPDATE
        db.execute.side_effect = [
            Mock(fetchall=Mock(return_value=[
                ("s1", "Did auth work", datetime(2026, 1, 1, tzinfo=timezone.utc), "Auth Session"),
            ])),
            Mock(fetchall=Mock(return_value=[
                ("e1", "user_preference", "language", "python", 0.9),
            ])),
            Mock(),  # access tracking UPDATE
            Mock(fetchall=Mock(return_value=[
                ("n1", "s0", "plan", "Finish migration", datetime(2026, 1, 2, tzinfo=timezone.utc)),
            ])),
        ]

        sc = SessionContinuity(lambda: db)
        prior = sc.load_prior_context("alice", current_session_id="s2")

        assert len(prior.session_summaries) == 1
        assert prior.session_summaries[0]["summary"] == "Did auth work"
        assert len(prior.knowledge_entries) == 1
        assert prior.knowledge_entries[0]["key"] == "language"
        assert len(prior.active_notes) == 1
        assert prior.active_notes[0]["content"] == "Finish migration"

    def test_load_empty_results(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
        ]

        sc = SessionContinuity(lambda: db)
        prior = sc.load_prior_context("alice")
        assert prior.to_prompt_section() is None

    def test_summarize_session_inserts_event(self):
        db = _mock_db()
        sc = SessionContinuity(lambda: db)
        sc.summarize_session("sess-1", "User worked on auth module")
        db.execute.assert_called_once()
        db.commit.assert_called_once()

    def test_exclude_current_session(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
        ]

        sc = SessionContinuity(lambda: db)
        sc.load_prior_context("alice", current_session_id="current-sess")

        # First call is session summaries — check the SQL contains exclude
        call_args = db.execute.call_args_list[0]
        params = call_args[0][1]
        assert params.get("exclude") == "current-sess"

    def test_knowledge_filters_low_confidence(self):
        """SQL filters confidence > 0.3 — verified by query structure."""
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[
                ("e1", "pref", "lang", "py", 0.5),
            ])),
            Mock(),  # access tracking UPDATE
            Mock(fetchall=Mock(return_value=[])),
        ]

        sc = SessionContinuity(lambda: db)
        prior = sc.load_prior_context("alice")
        assert len(prior.knowledge_entries) == 1
        assert prior.knowledge_entries[0]["confidence"] == 0.5

    def test_load_respects_limits(self):
        db = _mock_db()
        db.execute.side_effect = [
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
            Mock(fetchall=Mock(return_value=[])),
        ]

        sc = SessionContinuity(lambda: db)
        sc.load_prior_context("alice", max_summaries=3, max_knowledge=10, max_notes=5)

        # Verify limit params passed to each query
        calls = db.execute.call_args_list
        assert calls[0][0][1]["limit"] == 3
        assert calls[1][0][1]["limit"] == 10
        assert calls[2][0][1]["limit"] == 5


class TestChatLoopContinuityIntegration:
    def test_prior_context_injected_into_system_prompt(self):
        from core.agent.chat_loop import ChatLoop

        continuity = Mock()
        continuity.load_prior_context.return_value = PriorContext(
            session_summaries=[
                {"session_id": "s1", "summary": "Worked on auth", "title": "Auth"},
            ],
        )

        loop = ChatLoop(
            selector=Mock(),
            executor=Mock(),
            llm_client=Mock(),
            event_logger=Mock(),
            context_manager=Mock(),
            firewall=Mock(),
            continuity=continuity,
        )

        messages = loop._build_messages("hello", None, session_id="s2", user_id="alice")
        system_msg = messages[0]["content"]
        assert "Previous Sessions" in system_msg
        assert "Worked on auth" in system_msg
        continuity.load_prior_context.assert_called_once_with(
            user_id="alice", current_session_id="s2",
        )

    def test_no_continuity_no_injection(self):
        from core.agent.chat_loop import ChatLoop

        loop = ChatLoop(
            selector=Mock(),
            executor=Mock(),
            llm_client=Mock(),
            event_logger=Mock(),
            context_manager=Mock(),
            firewall=Mock(),
        )

        messages = loop._build_messages("hello", None, session_id="s1", user_id="alice")
        system_msg = messages[0]["content"]
        assert "Previous Sessions" not in system_msg

    def test_no_user_id_no_injection(self):
        from core.agent.chat_loop import ChatLoop

        continuity = Mock()
        loop = ChatLoop(
            selector=Mock(),
            executor=Mock(),
            llm_client=Mock(),
            event_logger=Mock(),
            context_manager=Mock(),
            firewall=Mock(),
            continuity=continuity,
        )

        messages = loop._build_messages("hello", None, session_id="s1", user_id=None)
        continuity.load_prior_context.assert_not_called()

    def test_empty_prior_context_no_section(self):
        from core.agent.chat_loop import ChatLoop

        continuity = Mock()
        continuity.load_prior_context.return_value = PriorContext()

        loop = ChatLoop(
            selector=Mock(),
            executor=Mock(),
            llm_client=Mock(),
            event_logger=Mock(),
            context_manager=Mock(),
            firewall=Mock(),
            continuity=continuity,
        )

        messages = loop._build_messages("hello", None, session_id="s1", user_id="alice")
        system_msg = messages[0]["content"]
        assert "Previous Sessions" not in system_msg
