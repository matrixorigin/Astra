"""Memory governance configuration."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class MemoryGovernanceConfig:
    """Configurable parameters for memory governance."""

    pitr_range_value: int = 14
    pitr_range_unit: str = "d"
    milestone_snapshot_keep_n: int = 5
    pollution_threshold: float = 0.3
    sandbox_enabled_types: tuple[str, ...] = ("profile",)
    confidence_decay_half_life_days: float = 30.0
    reflector_cluster_similarity: float = 0.7
    reflector_cluster_min_size: int = 3
    contradiction_similarity_threshold: float = 0.85
    
    # TOOL_RESULT specific TTL (independent of confidence decay)
    tool_result_ttl_hours: int = 24  # Auto-cleanup after session close + 24h
    tool_result_max_per_session: int = 100  # Quota per session
    tool_result_cleanup_on_session_close: bool = True  # Cleanup when session closes


# Default config instance
DEFAULT_CONFIG = MemoryGovernanceConfig()
