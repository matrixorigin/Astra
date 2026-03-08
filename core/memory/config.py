"""Memory governance configuration."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class MemoryGovernanceConfig:
    """Configurable parameters for memory governance.

    All timing, threshold, and decay parameters in one place.
    Can be loaded from DB/env at startup for dynamic adjustment.
    """

    pitr_range_value: int = 14
    pitr_range_unit: str = "d"
    milestone_snapshot_keep_n: int = 5
    pollution_threshold: float = 0.3
    sandbox_enabled_types: tuple[str, ...] = ("profile",)
    contradiction_similarity_threshold: float = 0.85

    # ── Cleanup TTLs ──
    tool_result_ttl_hours: int = 24
    tool_result_max_per_session: int = 100
    tool_result_cleanup_on_session_close: bool = True
    working_memory_stale_hours: int = 2

    # ── Confidence decay (per trust tier) ──
    half_life_t1_days: float = 365.0
    half_life_t2_days: float = 180.0
    half_life_t3_days: float = 60.0
    half_life_t4_days: float = 30.0

    # ── Quarantine ──
    quarantine_threshold: float = 0.3

    # ── Session summary ──
    session_summary_turn_threshold: int = 10
    session_summary_time_threshold_hours: float = 10 / 60  # 10 minutes

    # ── Reflection: candidate selection ──
    cluster_similarity_threshold: float = 0.8
    min_cross_session_count: int = 2
    min_summary_recurrence: int = 3
    summary_recurrence_window_days: int = 7

    # ── Reflection: importance scoring ──
    reflection_daily_threshold: float = 0.5
    reflection_immediate_threshold: float = 0.7

    # ── Opinion evolution ──
    opinion_supporting_delta: float = 0.05
    opinion_contradicting_delta: float = -0.10
    opinion_confidence_cap: float = 0.95
    opinion_supporting_threshold: float = 0.8
    opinion_contradicting_threshold: float = 0.3
    opinion_quarantine_threshold: float = 0.2
    opinion_t4_to_t3_confidence: float = 0.8
    opinion_t4_to_t3_min_age_days: int = 7

    # ── Distributed: run_daily_all sharding ──
    daily_batch_size: int = 2000
    shard_index: int = 0       # this worker's shard (0-based)
    shard_count: int = 1       # total workers (1 = no sharding)

    # ── Backend selector ──
    memory_backend: str = "tabular"

    @property
    def half_lives(self) -> dict[str, float]:
        """Return half-life mapping keyed by tier value string."""
        return {
            "T1": self.half_life_t1_days,
            "T2": self.half_life_t2_days,
            "T3": self.half_life_t3_days,
            "T4": self.half_life_t4_days,
        }


# Default config instance
DEFAULT_CONFIG = MemoryGovernanceConfig()
