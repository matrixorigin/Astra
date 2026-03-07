"""Memory governance configuration."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass
class MemoryGovernanceConfig:
    """Configurable parameters for memory governance."""

    pitr_range_value: int = 14
    pitr_range_unit: str = "d"
    milestone_snapshot_keep_n: int = 5
    pollution_threshold: float = 0.3
    sandbox_enabled_types: tuple[str, ...] = ("profile",)
    confidence_decay_half_life_days: float = 30.0
    contradiction_similarity_threshold: float = 0.85

    # TOOL_RESULT specific TTL
    tool_result_ttl_hours: int = 24
    tool_result_max_per_session: int = 100
    tool_result_cleanup_on_session_close: bool = True

    # Quarantine threshold (effective_confidence below this → deactivate)
    quarantine_threshold: float = 0.3

    # Working memory archival (hours of inactivity before archival)
    working_memory_stale_hours: int = 2

    # Session summary thresholds
    session_summary_turn_threshold: int = 10
    session_summary_time_threshold_hours: float = 10 / 60  # 10 minutes

    # Backend selector: "tabular" or "graph"
    memory_backend: str = "tabular"


# Default config instance
DEFAULT_CONFIG = MemoryGovernanceConfig()
