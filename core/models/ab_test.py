"""A/B test router for model versions.

Routes requests to different model versions based on session-level hashing.
Ensures same session always gets same model (deterministic).
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class ABTestConfig:
    """A/B test configuration."""

    experiment_name: str
    control_artifact_id: str  # current active model
    treatment_artifact_id: str  # new candidate model
    treatment_pct: int = 10  # percentage routed to treatment (0-100)

    def __post_init__(self) -> None:
        self.treatment_pct = max(0, min(100, self.treatment_pct))


@dataclass
class ABTestResult:
    group: str  # "control" | "treatment"
    artifact_id: str


class ABTestRouter:
    """Route sessions to model variants for A/B testing."""

    def __init__(self) -> None:
        self._experiments: dict[str, ABTestConfig] = {}

    def register(self, config: ABTestConfig) -> None:
        self._experiments[config.experiment_name] = config
        logger.info(
            f"Registered A/B test '{config.experiment_name}': "
            f"{config.treatment_pct}% → {config.treatment_artifact_id}"
        )

    def remove(self, experiment_name: str) -> bool:
        return self._experiments.pop(experiment_name, None) is not None

    def route(self, experiment_name: str, session_id: str) -> ABTestResult | None:
        """Route a session to control or treatment. Returns None if no experiment."""
        config = self._experiments.get(experiment_name)
        if not config:
            return None

        bucket = _hash_bucket(session_id, config.experiment_name)
        if bucket < config.treatment_pct:
            return ABTestResult(group="treatment", artifact_id=config.treatment_artifact_id)
        return ABTestResult(group="control", artifact_id=config.control_artifact_id)

    def list_experiments(self) -> list[str]:
        return list(self._experiments.keys())


def _hash_bucket(session_id: str, salt: str) -> int:
    """Deterministic 0-99 bucket from session_id + salt."""
    h = hashlib.sha256(f"{session_id}:{salt}".encode()).hexdigest()
    return int(h[:8], 16) % 100
