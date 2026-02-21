"""Retraining monitor — detect when feedback classifier needs retraining.

Triggers:
  1. Data growth: llm_feedback count increased ≥20% since last training
  2. Staleness: no retraining in 30 days
  3. Manual: mo-admin feedback retrain
"""

from __future__ import annotations

from datetime import datetime, timezone, timedelta

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)

_GROWTH_THRESHOLD = 0.20  # 20% data growth triggers retrain
_STALENESS_DAYS = 30


class RetrainingMonitor:
    """Monitor feedback data and decide if retraining is needed."""

    def __init__(self, db: Session) -> None:
        self.db = db

    def should_retrain(self) -> dict:
        """Check all triggers. Returns {needed: bool, reason: str, stats: dict}."""
        current_count = self._feedback_count()
        last_artifact = self._last_artifact()

        if not last_artifact:
            if current_count >= 50:
                return {"needed": True, "reason": "no_model_exists", "stats": {"feedback_count": current_count}}
            return {"needed": False, "reason": "insufficient_data", "stats": {"feedback_count": current_count}}

        last_size = last_artifact["dataset_size"] or 0
        last_date = last_artifact["created_at"]

        stats = {
            "feedback_count": current_count,
            "last_train_size": last_size,
            "growth_pct": round((current_count - last_size) / max(last_size, 1) * 100, 1),
            "days_since_train": (datetime.now(timezone.utc) - _to_utc(last_date)).days if last_date else None,
        }

        # Check data growth
        if last_size > 0 and (current_count - last_size) / last_size >= _GROWTH_THRESHOLD:
            return {"needed": True, "reason": "data_growth", "stats": stats}

        # Check staleness
        if last_date:
            age = datetime.now(timezone.utc) - _to_utc(last_date)
            if age > timedelta(days=_STALENESS_DAYS):
                return {"needed": True, "reason": "stale_model", "stats": stats}

        return {"needed": False, "reason": "up_to_date", "stats": stats}

    def _feedback_count(self) -> int:
        row = self.db.execute(text("SELECT COUNT(*) FROM llm_feedback")).fetchone()
        return row[0] if row else 0

    def _last_artifact(self) -> dict | None:
        row = self.db.execute(text("""
            SELECT artifact_id, dataset_size, created_at
            FROM model_artifacts
            WHERE model_name = 'feedback_classifier'
            ORDER BY created_at DESC LIMIT 1
        """)).fetchone()
        if not row:
            return None
        return {"artifact_id": row[0], "dataset_size": row[1], "created_at": row[2]}


def _to_utc(dt) -> datetime:
    if dt is None:
        return datetime.now(timezone.utc)
    if isinstance(dt, str):
        dt = datetime.fromisoformat(dt)
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt
