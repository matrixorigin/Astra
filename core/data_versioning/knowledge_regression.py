"""Knowledge regression detection — P2 Evaluation Loop.

Detects when knowledge updates invalidate past outputs.
"""

from dataclasses import dataclass
from datetime import datetime
from enum import Enum


class RegressionType(Enum):
    """Type of knowledge regression."""
    INVALIDATED = "invalidated"
    CONTRADICTED = "contradicted"
    OUTDATED = "outdated"
    SKILL_DEPRECATED = "skill_deprecated"


@dataclass
class RegressionSignal:
    """Signal indicating potential regression."""
    signal_id: str
    regression_type: RegressionType
    affected_skill: str
    affected_sessions: int
    affected_decisions: int
    confidence: float
    detected_at: datetime
    metadata: dict | None = None


@dataclass
class RegressionReport:
    """Report of detected regressions."""
    report_id: str
    signals: list[RegressionSignal]
    total_affected_sessions: int
    total_affected_decisions: int
    generated_at: datetime


class KnowledgeRegression:
    """Detects knowledge regressions in past outputs."""
    
    def __init__(self, db_factory):
        self.db_factory = db_factory
    
    def detect(self, knowledge_update_id: str) -> RegressionReport:
        """Detect regressions caused by knowledge update."""
        from datetime import datetime, timezone
        # Stub implementation
        return RegressionReport(
            report_id="stub",
            signals=[],
            total_checked=0,
            total_flagged=0,
            generated_at=datetime.now(timezone.utc)
        )

