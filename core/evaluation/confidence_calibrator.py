"""Confidence calibration — measure and correct prediction accuracy.

Ref: evaluation-and-evolution.md §5 "Confidence Calibration"

Compares pre-delivery confidence_score (from firewall) against
post-delivery quality_score (from evaluation) to detect systematic
over/under-confidence and compute calibration coefficients.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class CalibrationResult:
    mean_confidence: float
    mean_quality: float
    calibration_error: float  # |confidence - quality| averaged
    bias: float               # positive = overconfident, negative = underconfident
    sample_count: int
    bucket_errors: list[dict[str, Any]]  # per-bucket breakdown


class ConfidenceCalibrator:
    """Measures and corrects confidence calibration."""

    BUCKETS = 5  # split confidence range [0,1] into N buckets
    RECALIBRATION_THRESHOLD = 0.15  # trigger recalibration if error > this

    def __init__(self, db: Session):
        self.db = db

    def measure(self, agent_id: str | None = None, days: int = 30) -> CalibrationResult:
        """Compute calibration error from historical data.

        Compares firewall confidence_score against quality_score
        (normalized to [0,1] from [0,5]).
        """
        rows = self._query_pairs(agent_id, days)
        if not rows:
            return CalibrationResult(
                mean_confidence=0.0, mean_quality=0.0,
                calibration_error=0.0, bias=0.0,
                sample_count=0, bucket_errors=[],
            )

        confidences = [r[0] for r in rows]
        qualities = [r[1] / 5.0 for r in rows]  # normalize to [0,1]

        mean_conf = sum(confidences) / len(confidences)
        mean_qual = sum(qualities) / len(qualities)
        bias = mean_conf - mean_qual

        # Expected Calibration Error (ECE) — bucket-based
        bucket_errors = self._compute_ece(confidences, qualities)
        ece = sum(b["weighted_error"] for b in bucket_errors)

        return CalibrationResult(
            mean_confidence=round(mean_conf, 4),
            mean_quality=round(mean_qual, 4),
            calibration_error=round(ece, 4),
            bias=round(bias, 4),
            sample_count=len(rows),
            bucket_errors=bucket_errors,
        )

    def compute_adjustment(self, result: CalibrationResult) -> dict[str, float]:
        """Compute weight adjustments based on calibration error.

        Returns multiplier for firewall confidence weights.
        """
        if result.sample_count < 20:
            return {"multiplier": 1.0, "reason": "insufficient_data"}

        if abs(result.bias) < 0.05:
            return {"multiplier": 1.0, "reason": "well_calibrated"}

        # Overconfident: scale down. Underconfident: scale up.
        # Damped correction: move 50% toward perfect calibration
        multiplier = 1.0 - (result.bias * 0.5)
        multiplier = max(0.5, min(1.5, multiplier))  # clamp

        return {
            "multiplier": round(multiplier, 4),
            "bias": result.bias,
            "reason": "overconfident" if result.bias > 0 else "underconfident",
        }

    def _query_pairs(
        self, agent_id: str | None, days: int,
    ) -> list[tuple[float, float]]:
        """Query (confidence_score, quality_score) pairs."""
        try:
            params: dict[str, Any] = {"days": days}
            agent_filter = ""
            if agent_id:
                agent_filter = "AND agent_id = :agent_id"
                params["agent_id"] = agent_id

            rows = self.db.execute(text(f"""
                SELECT
                    CAST(JSON_UNQUOTE(JSON_EXTRACT(`metadata`, '$.confidence_score')) AS DOUBLE) AS conf,
                    quality_score
                FROM conversation_events
                WHERE event_type = 'llm_response'
                  AND quality_score IS NOT NULL
                  AND `metadata` IS NOT NULL
                  AND JSON_EXTRACT(`metadata`, '$.confidence_score') IS NOT NULL
                  AND created_at >= DATE_SUB(NOW(), INTERVAL :days DAY)
                  {agent_filter}
            """), params).fetchall()

            return [(float(r[0]), float(r[1])) for r in rows if r[0] is not None]
        except Exception as e:
            logger.warning("Calibration query failed: %s", e)
            return []

    def _compute_ece(
        self, confidences: list[float], qualities: list[float],
    ) -> list[dict[str, Any]]:
        """Expected Calibration Error with bucket breakdown."""
        n = len(confidences)
        buckets: list[dict[str, Any]] = []

        for i in range(self.BUCKETS):
            lo = i / self.BUCKETS
            hi = (i + 1) / self.BUCKETS
            indices = [
                j for j, c in enumerate(confidences) if lo <= c < hi
            ]
            if not indices:
                buckets.append({
                    "range": f"[{lo:.1f}, {hi:.1f})",
                    "count": 0, "avg_confidence": 0, "avg_quality": 0,
                    "error": 0, "weighted_error": 0,
                })
                continue

            avg_conf = sum(confidences[j] for j in indices) / len(indices)
            avg_qual = sum(qualities[j] for j in indices) / len(indices)
            error = abs(avg_conf - avg_qual)

            buckets.append({
                "range": f"[{lo:.1f}, {hi:.1f})",
                "count": len(indices),
                "avg_confidence": round(avg_conf, 4),
                "avg_quality": round(avg_qual, 4),
                "error": round(error, 4),
                "weighted_error": round(error * len(indices) / n, 4),
            })

        return buckets
