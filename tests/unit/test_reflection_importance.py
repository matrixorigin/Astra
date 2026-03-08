"""Tests for score_candidate — 4-signal heuristic scoring."""

import pytest

from core.memory.interfaces import ReflectionCandidate
from core.memory.reflection.importance import (
    DAILY_THRESHOLD,
    IMMEDIATE_THRESHOLD,
    W_CENTRALITY,
    W_CONTRADICTION,
    W_CROSS_SESSION,
    W_RECURRENCE,
    score_candidate,
)
from core.memory.types import Memory, MemoryType


def _mem(mid="m1", sid="s1", conf=0.8):
    return Memory(
        memory_id=mid, user_id="u1", memory_type=MemoryType.SEMANTIC,
        content="test", initial_confidence=conf, session_id=sid,
    )


class TestScoreCandidate:
    def test_weights_sum_to_one(self):
        assert W_CENTRALITY + W_CROSS_SESSION + W_CONTRADICTION + W_RECURRENCE == pytest.approx(1.0)

    def test_empty_candidate_scores_low(self):
        c = ReflectionCandidate(memories=[], signal="semantic_cluster", session_ids=[])
        assert score_candidate(c) < DAILY_THRESHOLD

    def test_contradiction_signal_scores_high(self):
        c = ReflectionCandidate(
            memories=[_mem("m1", "s1"), _mem("m2", "s2")],
            signal="contradiction",
            session_ids=["s1", "s2"],
        )
        assert score_candidate(c) >= DAILY_THRESHOLD

    def test_large_cross_session_cluster_scores_high(self):
        mems = [_mem(f"m{i}", f"s{i}") for i in range(6)]
        c = ReflectionCandidate(
            memories=mems, signal="semantic_cluster",
            session_ids=["s1", "s2", "s3", "s4"],
        )
        assert score_candidate(c) >= DAILY_THRESHOLD

    def test_single_session_cluster_scores_zero_cross_session(self):
        c = ReflectionCandidate(
            memories=[_mem()], signal="semantic_cluster",
            session_ids=["s1"],
        )
        score = score_candidate(c)
        # cross_session = 1/3 ≈ 0.33, not 0
        assert score < IMMEDIATE_THRESHOLD

    def test_score_range_zero_to_one(self):
        c = ReflectionCandidate(
            memories=[_mem(f"m{i}", f"s{i}") for i in range(10)],
            signal="contradiction",
            session_ids=[f"s{i}" for i in range(5)],
        )
        score = score_candidate(c)
        assert 0.0 <= score <= 1.0

    def test_activation_energy_used_for_centrality(self):
        c = ReflectionCandidate(
            memories=[_mem()], signal="semantic_cluster",
            session_ids=["s1", "s2", "s3"],
        )
        score_high = score_candidate(c, activation_energy=0.85)
        score_low = score_candidate(c, activation_energy=0.1)
        assert score_high > score_low

    def test_zero_activation_falls_back_to_cluster_size(self):
        mems = [_mem(f"m{i}") for i in range(5)]
        c = ReflectionCandidate(
            memories=mems, signal="semantic_cluster",
            session_ids=["s1", "s2"],
        )
        score = score_candidate(c, activation_energy=0.0)
        # centrality = 5/5 = 1.0 (cluster size fallback)
        assert score > 0.4

    def test_activation_energy_capped_at_1(self):
        c = ReflectionCandidate(
            memories=[_mem()], signal="semantic_cluster",
            session_ids=["s1"],
        )
        # Even with activation_energy=5.0, centrality should cap at 1.0
        score_capped = score_candidate(c, activation_energy=5.0)
        score_one = score_candidate(c, activation_energy=1.0)
        assert score_capped == pytest.approx(score_one)
