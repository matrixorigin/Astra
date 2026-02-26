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
    reflector_cluster_similarity: float = 0.7
    reflector_cluster_min_size: int = 3
    contradiction_similarity_threshold: float = 0.85


# Default config instance
DEFAULT_CONFIG = MemoryGovernanceConfig()
