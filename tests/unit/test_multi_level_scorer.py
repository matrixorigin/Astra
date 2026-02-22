"""Tests for three-level quality evaluation (chain + session)."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest

from core.evaluation.multi_level_scorer import (
    _CASCADE_PENALTY_FACTOR,
    _CASCADE_PENALTY_THRESHOLD,
    score_chain,
    score_session,
)


# ── Helpers ──────────────────────────────────────────────────────

def _make_row(**kwargs):
    """Create a mock DB row with attribute access."""
    m = MagicMock()
    for k, v in kwargs.items():
        setattr(m, k, v)
    return m


def _mock_db_for_chain(scores: list[float]):
    """Return a mock db whose execute returns rows with quality_score."""
    db = MagicMock()
    rows = [_make_row(event_id=f"e{i}", quality_score=s) for i, s in enumerate(scores)]
    # 1st call = step query, 2nd = upsert SELECT (no existing row), 3rd = upsert INSERT
    call_results = iter([
        MagicMock(fetchall=MagicMock(return_value=rows)),  # step query
        MagicMock(fetchone=MagicMock(return_value=None)),   # upsert SELECT
        MagicMock(),                                        # upsert INSERT
    ])
    db.execute = MagicMock(side_effect=lambda *a, **kw: next(call_results))
    return db


# ── Chain-level tests ────────────────────────────────────────────

class TestScoreChain:
    def test_no_scored_steps_returns_none(self):
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        assert score_chain(db, "chain1", "sess1") is None

    def test_single_step(self):
        db = _mock_db_for_chain([4.0])
        result = score_chain(db, "chain1", "sess1")
        assert result is not None
        assert result["step_count"] == 1
        assert result["failure_count"] == 0
        assert result["score"] == 4.0

    def test_cascade_penalty_applied(self):
        """A failing step (< 2.5) triggers cascade penalty."""
        db = _mock_db_for_chain([4.0, 1.0])  # second step fails
        result = score_chain(db, "chain1", "sess1")
        assert result["failure_count"] == 1
        # penalty = 1 * 0.15 = 0.15
        assert result["details"]["cascade_penalty"] == _CASCADE_PENALTY_FACTOR

    def test_multiple_failures_compound(self):
        db = _mock_db_for_chain([1.0, 1.5, 4.0])
        result = score_chain(db, "chain1", "sess1")
        assert result["failure_count"] == 2
        assert result["details"]["cascade_penalty"] == round(2 * _CASCADE_PENALTY_FACTOR, 2)

    def test_all_high_scores_no_penalty(self):
        db = _mock_db_for_chain([4.5, 4.0, 5.0])
        result = score_chain(db, "chain1", "sess1")
        assert result["failure_count"] == 0
        assert result["details"]["cascade_penalty"] == 0.0
        assert result["score"] > 4.0

    def test_later_steps_weighted_more(self):
        """Later steps have higher weight, so a bad last step hurts more."""
        db1 = _mock_db_for_chain([1.0, 5.0])  # bad first, good last
        r1 = score_chain(db1, "c1", "s1")
        db2 = _mock_db_for_chain([5.0, 1.0])  # good first, bad last
        r2 = score_chain(db2, "c2", "s1")
        # Both have 1 failure, same penalty. But base differs due to weighting.
        assert r1["details"]["base_score"] > r2["details"]["base_score"]

    def test_score_clamped_0_5(self):
        db = _mock_db_for_chain([0.0, 0.0, 0.0])
        result = score_chain(db, "chain1", "sess1")
        assert 0.0 <= result["score"] <= 5.0

    def test_upsert_commits(self):
        """Upsert always commits after INSERT ON DUPLICATE KEY."""
        db = _mock_db_for_chain([4.0])
        score_chain(db, "chain1", "sess1")
        db.commit.assert_called()


# ── Session-level tests ──────────────────────────────────────────

class TestScoreSession:
    def test_no_chains_returns_none(self):
        db = MagicMock()
        db.execute.return_value.fetchall.return_value = []
        assert score_session(db, "sess1") is None

    def test_single_chain(self):
        db = MagicMock()
        chain_row = _make_row(target_id="c1", score=4.0, step_count=3, failure_count=0)
        call_results = iter([
            MagicMock(fetchall=MagicMock(return_value=[chain_row])),  # chain query
            MagicMock(fetchone=MagicMock(return_value=None)),          # upsert SELECT
            MagicMock(),                                               # upsert INSERT
        ])
        db.execute = MagicMock(side_effect=lambda *a, **kw: next(call_results))
        result = score_session(db, "sess1")
        assert result is not None
        assert result["score"] == 4.0
        assert result["chain_count"] == 1

    def test_weighted_by_step_count(self):
        """Longer chains contribute more to session score."""
        db = MagicMock()
        chains = [
            _make_row(target_id="c1", score=5.0, step_count=1, failure_count=0),
            _make_row(target_id="c2", score=2.0, step_count=9, failure_count=2),
        ]
        call_results = iter([
            MagicMock(fetchall=MagicMock(return_value=chains)),
            MagicMock(fetchone=MagicMock(return_value=None)),  # upsert SELECT
            MagicMock(),                                        # upsert INSERT
        ])
        db.execute = MagicMock(side_effect=lambda *a, **kw: next(call_results))
        result = score_session(db, "sess1")
        # (5*1 + 2*9) / 10 = 2.3 — dominated by the longer chain
        assert result["score"] == 2.3


# ── ChatLoop wiring test ─────────────────────────────────────────

class TestChatLoopChainScoring:
    def test_log_response_triggers_chain_scoring(self):
        """_log_response calls score_chain after auto-scoring."""
        from core.agent.chat_loop import ChatLoop

        loop = ChatLoop.__new__(ChatLoop)
        loop.event_logger = MagicMock()
        loop.event_logger.create_llm_response.return_value = MagicMock(event_id="e1")
        loop.llm = MagicMock()
        loop.llm.config = {"model": "gpt-4"}

        with patch("core.evaluation.multi_level_scorer.score_chain") as mock_sc:
            loop._log_response(
                user_id="u1", session_id="s1", content="hello",
                parent_event_id="p1", causal_chain_id="chain1",
            )
            mock_sc.assert_called_once_with(
                loop.event_logger.session, "chain1", "s1",
            )

    def test_chain_scoring_skipped_when_no_chain_id(self):
        from core.agent.chat_loop import ChatLoop

        loop = ChatLoop.__new__(ChatLoop)
        loop.event_logger = MagicMock()
        loop.event_logger.create_llm_response.return_value = MagicMock(event_id="e1")
        loop.llm = MagicMock()
        loop.llm.config = {"model": "gpt-4"}

        with patch("core.evaluation.multi_level_scorer.score_chain") as mock_sc:
            loop._log_response(
                user_id="u1", session_id="s1", content="hello",
                parent_event_id="p1", causal_chain_id=None,
            )
            mock_sc.assert_not_called()

    def test_chain_scoring_failure_non_fatal(self):
        from core.agent.chat_loop import ChatLoop

        loop = ChatLoop.__new__(ChatLoop)
        loop.event_logger = MagicMock()
        loop.event_logger.create_llm_response.return_value = MagicMock(event_id="e1")
        loop.llm = MagicMock()
        loop.llm.config = {"model": "gpt-4"}

        with patch("core.evaluation.multi_level_scorer.score_chain", side_effect=RuntimeError("boom")):
            # Should not raise
            loop._log_response(
                user_id="u1", session_id="s1", content="hello",
                parent_event_id="p1", causal_chain_id="chain1",
            )


# ── Structural test ──────────────────────────────────────────────

class TestStructural:
    def test_score_chain_called_in_log_response(self):
        """Verify score_chain import exists in _log_response."""
        import inspect
        from core.agent.chat_loop import ChatLoop
        source = inspect.getsource(ChatLoop._log_response)
        assert "score_chain" in source
