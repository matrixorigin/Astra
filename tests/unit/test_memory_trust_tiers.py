"""Unit tests for trust tier support."""

from datetime import datetime, timezone, timedelta
from unittest.mock import MagicMock, patch

import pytest

from core.memory.types import Memory, MemoryType, TrustTier, TRUST_TIER_HALF_LIVES


class TestTrustTierEnum:
    def test_values(self):
        assert TrustTier.T1_VERIFIED.value == "T1"
        assert TrustTier.T2_CURATED.value == "T2"
        assert TrustTier.T3_INFERRED.value == "T3"
        assert TrustTier.T4_UNVERIFIED.value == "T4"

    def test_half_lives(self):
        assert TRUST_TIER_HALF_LIVES[TrustTier.T1_VERIFIED] == 365.0
        assert TRUST_TIER_HALF_LIVES[TrustTier.T4_UNVERIFIED] == 30.0

    def test_from_string(self):
        assert TrustTier("T1") == TrustTier.T1_VERIFIED


class TestTrustTierDecay:
    def test_default_tier_is_t3(self):
        mem = Memory(memory_id="m1", user_id="u1", memory_type=MemoryType.SEMANTIC, content="x")
        assert mem.trust_tier == TrustTier.T3_INFERRED

    def test_t1_decays_slower_than_t4(self):
        """Same age, same initial_confidence — T1 should have higher effective_confidence."""
        age = datetime.now(timezone.utc) - timedelta(days=60)
        t1 = Memory(memory_id="t1", user_id="u", memory_type=MemoryType.SEMANTIC,
                     content="x", initial_confidence=1.0, observed_at=age,
                     trust_tier=TrustTier.T1_VERIFIED)
        t4 = Memory(memory_id="t4", user_id="u", memory_type=MemoryType.SEMANTIC,
                     content="x", initial_confidence=1.0, observed_at=age,
                     trust_tier=TrustTier.T4_UNVERIFIED)
        # T1: 365d half-life → barely decayed at 60d
        # T4: 30d half-life → ~0.13 at 60d (2 half-lives)
        assert t1.effective_confidence() > 0.8
        assert t4.effective_confidence() < 0.2
        assert t1.effective_confidence() > t4.effective_confidence()

    def test_explicit_half_life_overrides_tier(self):
        mem = Memory(memory_id="m1", user_id="u", memory_type=MemoryType.SEMANTIC,
                     content="x", initial_confidence=1.0,
                     observed_at=datetime.now(timezone.utc) - timedelta(days=30),
                     trust_tier=TrustTier.T1_VERIFIED)
        # Override T1's 365d with 30d
        ec = mem.effective_confidence(half_life_days=30.0)
        assert ec < 0.5  # ~0.37 at 1 half-life

    def test_no_observed_at_returns_initial(self):
        mem = Memory(memory_id="m1", user_id="u", memory_type=MemoryType.SEMANTIC,
                     content="x", initial_confidence=0.8, trust_tier=TrustTier.T4_UNVERIFIED)
        assert mem.effective_confidence() == 0.8
