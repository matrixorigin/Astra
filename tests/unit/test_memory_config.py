"""Unit tests for MemoryGovernanceConfig — from_env() and half_lives."""

import os
from unittest.mock import patch

import pytest

from core.memory.config import MemoryGovernanceConfig


class TestFromEnvFloat:
    def test_overrides_float_field(self):
        with patch.dict(os.environ, {"MEM_HALF_LIFE_T1_DAYS": "400"}):
            c = MemoryGovernanceConfig.from_env()
        assert c.half_life_t1_days == 400.0

    def test_default_when_no_env(self):
        with patch.dict(os.environ, {}, clear=True):
            c = MemoryGovernanceConfig.from_env()
        assert c.half_life_t1_days == 365.0


class TestFromEnvInt:
    def test_overrides_int_field(self):
        with patch.dict(os.environ, {"MEM_SHARD_COUNT": "8"}):
            c = MemoryGovernanceConfig.from_env()
        assert c.shard_count == 8

    def test_overrides_shard_index(self):
        with patch.dict(os.environ, {"MEM_SHARD_INDEX": "3"}):
            c = MemoryGovernanceConfig.from_env()
        assert c.shard_index == 3


class TestFromEnvBool:
    @pytest.mark.parametrize("val,expected", [
        ("1", True), ("true", True), ("yes", True),
        ("0", False), ("false", False), ("no", False),
    ])
    def test_bool_values(self, val, expected):
        with patch.dict(os.environ, {"MEM_TOOL_RESULT_CLEANUP_ON_SESSION_CLOSE": val}):
            c = MemoryGovernanceConfig.from_env()
        assert c.tool_result_cleanup_on_session_close is expected


class TestFromEnvStr:
    def test_overrides_str_field(self):
        with patch.dict(os.environ, {"MEM_MEMORY_BACKEND": "graph"}):
            c = MemoryGovernanceConfig.from_env()
        assert c.memory_backend == "graph"


class TestFromEnvMultiple:
    def test_multiple_overrides(self):
        env = {
            "MEM_HALF_LIFE_T1_DAYS": "500",
            "MEM_QUARANTINE_THRESHOLD": "0.15",
            "MEM_DAILY_BATCH_SIZE": "5000",
        }
        with patch.dict(os.environ, env):
            c = MemoryGovernanceConfig.from_env()
        assert c.half_life_t1_days == 500.0
        assert c.quarantine_threshold == 0.15
        assert c.daily_batch_size == 5000
        # Unset fields keep defaults
        assert c.half_life_t2_days == 180.0


class TestFromEnvIgnoresUnknown:
    def test_unknown_env_var_ignored(self):
        with patch.dict(os.environ, {"MEM_NONEXISTENT_FIELD": "42"}):
            c = MemoryGovernanceConfig.from_env()
        assert not hasattr(c, "nonexistent_field")


class TestHalfLivesProperty:
    def test_reflects_config_values(self):
        c = MemoryGovernanceConfig(
            half_life_t1_days=100, half_life_t2_days=50,
            half_life_t3_days=25, half_life_t4_days=10,
        )
        assert c.half_lives == {"T1": 100, "T2": 50, "T3": 25, "T4": 10}

    def test_from_env_reflected_in_half_lives(self):
        with patch.dict(os.environ, {"MEM_HALF_LIFE_T4_DAYS": "7"}):
            c = MemoryGovernanceConfig.from_env()
        assert c.half_lives["T4"] == 7.0
        assert c.half_lives["T1"] == 365.0  # default


class TestRetrieverUsesConfig:
    """Verify MemoryRetriever._score_candidate uses config half-lives."""

    def test_custom_config_changes_confidence_score(self):
        from datetime import datetime, timezone
        from unittest.mock import MagicMock
        from core.memory.tabular.retriever import MemoryRetriever

        db_factory = MagicMock()
        # T4 default half-life = 30 days
        default_cfg = MemoryGovernanceConfig()
        # T4 custom half-life = 1 day (decays much faster)
        fast_cfg = MemoryGovernanceConfig(half_life_t4_days=1.0)

        r_default = MemoryRetriever(db_factory, config=default_cfg)
        r_fast = MemoryRetriever(db_factory, config=fast_cfg)

        # Build a candidate 10 days old, T4 tier
        from dataclasses import dataclass
        from typing import Optional

        @dataclass
        class _C:
            memory_id: str = "m1"
            content: str = "x"
            memory_type: str = "semantic"
            initial_confidence: float = 0.8
            observed_at: object = None
            session_id: Optional[str] = None
            trust_tier: str = "T4"
            keyword_score: float = 0.0
            l2_dist: Optional[float] = None

        from core.memory.types import RetrievalWeights
        w = RetrievalWeights(vector=0, keyword=0, temporal=0, confidence=1.0)

        c = _C(observed_at=datetime(2026, 2, 20, tzinfo=timezone.utc))

        import time
        now = time.time()
        _, _, _, _, conf_default = r_default._score_candidate(c, w, now)
        _, _, _, _, conf_fast = r_fast._score_candidate(c, w, now)

        # With half-life=1 day and ~16 days old, confidence should be near 0
        # With half-life=30 days, confidence should be much higher
        assert conf_default > conf_fast
        assert conf_fast < 0.01  # essentially decayed to nothing
