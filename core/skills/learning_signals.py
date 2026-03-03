"""Learning signals for multi-dimensional skill selection improvement."""

from dataclasses import dataclass
from enum import Enum
from typing import Any


class SignalType(str, Enum):
    """Types of learning signals."""
    WRONG_SKILL = "wrong_skill"
    SLOW_EXECUTION = "slow_execution"
    HIGH_COST = "high_cost"
    LOW_SATISFACTION = "low_satisfaction"
    LOW_DATA_QUALITY = "low_data_quality"
    EXECUTION_TIME = "execution_time"  # Raw execution time data for learning
    STALE_CONTEXT = "stale_context"    # Topic shift caused irrelevant context selection


@dataclass
class LearningSignal:
    """A learning signal extracted from skill selection feedback."""
    
    signal_type: SignalType
    query_pattern: str
    wrong_skills: list[str]
    correct_skills: list[str]
    target_metrics: dict[str, float]  # e.g., {"time_ms": 500, "cost": 0.01}
    confidence: float = 10.0
    context_features: dict[str, Any] | None = None  # Optional context features
    
    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for storage."""
        return {
            "signal_type": self.signal_type.value,
            "query_pattern": self.query_pattern,
            "wrong_skills": self.wrong_skills,
            "correct_skills": self.correct_skills,
            "target_metrics": self.target_metrics,
            "confidence": self.confidence,
            "context_features": self.context_features,
        }


@dataclass
class SignalWeights:
    """Weights for multi-dimensional scoring."""
    
    accuracy: float = 0.4
    speed: float = 0.3
    cost: float = 0.2
    satisfaction: float = 0.1
    
    def __post_init__(self):
        """Validate weights sum to 1.0 and are in valid range."""
        # Check individual weights
        for name in ["accuracy", "speed", "cost", "satisfaction"]:
            value = getattr(self, name)
            if value < 0:
                raise ValueError(f"Weight '{name}' cannot be negative: {value}")
            if value > 1.0:
                raise ValueError(f"Weight '{name}' cannot exceed 1.0: {value}")
        
        # Check sum
        total = self.accuracy + self.speed + self.cost + self.satisfaction
        if abs(total - 1.0) > 0.01:
            raise ValueError(f"Weights must sum to 1.0, got {total}")
    
    def to_dict(self) -> dict[str, float]:
        """Convert to dictionary."""
        return {
            "accuracy": self.accuracy,
            "speed": self.speed,
            "cost": self.cost,
            "satisfaction": self.satisfaction,
        }


@dataclass
class SignalThresholds:
    """Thresholds for signal extraction."""
    
    slow_execution_ms: int = 5000  # 5 seconds
    high_cost_usd: float = 0.10  # $0.10
    low_satisfaction: int = 3  # < 3 stars (out of 5)
    low_data_quality: float = 0.5  # quality score < 0.5 triggers signal
    
    def to_dict(self) -> dict[str, float]:
        """Convert to dictionary."""
        return {
            "slow_execution_ms": self.slow_execution_ms,
            "high_cost_usd": self.high_cost_usd,
            "low_satisfaction": self.low_satisfaction,
            "low_data_quality": self.low_data_quality,
        }
