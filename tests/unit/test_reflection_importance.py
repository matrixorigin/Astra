"""Tests for ImportanceScorer — 4-signal heuristic scoring."""

import pytest

from core.memory.interfaces import ReflectionCandidate
from core.memory.reflection.importance import (
    DAILY_THRESHOLD,
    IMMEDIATE_THRESHOLD,
    ImportanceScorer,
    W_CENTRALITY,
    W_CONTRADICTION,
    W_CROSS_SESSION,
    W_RECURRENCE,
)
from core.memory.types import Memory, MemoryType


def _mem(mid: str = "m1", session_id: str | None = "s1") -> Memory:
    return Memory(
        memory_id=mid, user_id="u1", memory_type=MemoryType.SEMANTIC,
        content="test", session_id=session_id,
    )


class TestImportanceScorer:
    def setup_method(self):
        self.scorer = ImportanceScorer()

    def test_weights_sum_to_one(self):
        total = W_CENTRALITY + W_CROSS_SESSION + W_CONTRADICTION + W_RECURRENCE
        assert abs(total - 1.0) < 0.001

    def test_empty_candidate_scores_low(self):
        c = ReflectionCandidate(
            memories=[_mem()], signal="semantic_cluster", session_ids=["s1"],
        )
        score = self.scorer.score(c)
        assert score < DAILY_THRESHOLD

    def test_contradiction_signal_scores_high(self):
        c = ReflectionCandidate(
            memories=[_mem("m1", "s1"), _mem("m2", "s2")],
            signal="contradiction",
            importance_boost=0.3,
            session_ids=["s1", "s2"],
        )
        score = self.scorer.score(c)
        # contradiction=1.0*0.30 + cross_session=0.67*0.25 + centrality=0.4*0.25 + recurrence=0.4*0.20
        assert score >= DAILY_THRESHOLD

    def test_large_cross_session_cluster_scores_high(self):
        mems = [_mem(f"m{i}", f"s{i}") for i in range(5)]
        c = ReflectionCandidate(
            memories=mems, signal="semantic_cluster",
            session_ids=[f"s{i}" for i in range(5)],
        )
        score = self.scorer.score(c)
        assert score >= DAILY_THRESHOLD

    def test_summary_recurrence_with_many_memories(self):
        mems = [_mem(f"m{i}") for i in range(5)]
        c = ReflectionCandidate(
            memories=mems, signal="summary_recurrence",
            importance_boost=0.2, session_ids=[],
        )
        score = self.scorer.score(c)
        # recurrence = min(5/3, 1.0) = 1.0 * 0.20
        # centrality = min(5/5, 1.0) = 1.0 * 0.25
        # contradiction boost = min(0.2/0.3, 1.0) = 0.67 * 0.30
        # cross_session = 0 * 0.25
        assert score > 0.4

    def test_single_session_cluster_scores_zero_cross_session(self):
        c = ReflectionCandidate(
            memories=[_mem("m1", "s1"), _mem("m2", "s1")],
            signal="semantic_cluster",
            session_ids=["s1"],
        )
        score = self.scorer.score(c)
        # cross_session = min(1/3, 1.0) * 0.25 = 0.083
        assert score < DAILY_THRESHOLD

    def test_score_range_zero_to_one(self):
        """Score should always be in [0, 1]."""
        # Minimal candidate
        c_min = ReflectionCandidate(
            memories=[_mem()], signal="semantic_cluster", session_ids=[],
        )
        assert 0.0 <= self.scorer.score(c_min) <= 1.0

        # Maximal candidate
        mems = [_mem(f"m{i}", f"s{i}") for i in range(10)]
        c_max = ReflectionCandidate(
            memories=mems, signal="contradiction",
            importance_boost=0.3, session_ids=[f"s{i}" for i in range(10)],
        )
        assert 0.0 <= self.scorer.score(c_max) <= 1.0
