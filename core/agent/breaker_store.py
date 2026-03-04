"""Circuit breaker persistence — exponential cooldown across turns.

Table: tool_breaker_state
Tracks per-user tool failure streaks and cooldown windows.

Design:
- Load once at turn start (1 SELECT per turn)
- Mutate in-memory during turn
- Flush once at turn end (1 batch upsert per turn)
- No per-tool-call DB writes
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import TYPE_CHECKING

from sqlalchemy import Column, DateTime, Integer, String
from sqlalchemy.sql import func

from api.base import Base
from core.logging_config import get_logger

if TYPE_CHECKING:

    from sqlalchemy.orm import Session

logger = get_logger(__name__)


class ToolBreakerState(Base):
    """Persisted circuit breaker state per user+tool."""

    __tablename__ = "tool_breaker_state"

    user_id = Column(String(64), primary_key=True)
    tool_name = Column(String(128), primary_key=True)
    consecutive_failures = Column(Integer, nullable=False, default=0)
    last_failure_at = Column(DateTime, nullable=True)
    cooldown_until = Column(DateTime, nullable=True)
    updated_at = Column(DateTime, server_default=func.now())


# Exponential cooldown schedule: 5min → 30min → 2h
_COOLDOWN_SCHEDULE = [
    timedelta(minutes=5),
    timedelta(minutes=30),
    timedelta(hours=2),
]


@dataclass
class BreakerRecord:
    """In-memory representation of a breaker state row."""

    user_id: str
    tool_name: str
    consecutive_failures: int = 0
    last_failure_at: datetime | None = None
    cooldown_until: datetime | None = None
    dirty: bool = field(default=False, repr=False)  # Track if needs DB write

    @property
    def in_cooldown(self) -> bool:
        if self.cooldown_until is None:
            return False
        now = datetime.now(timezone.utc)
        # DB may return naive datetime — treat as UTC
        cooldown = self.cooldown_until
        if cooldown.tzinfo is None:
            cooldown = cooldown.replace(tzinfo=timezone.utc)
        return now < cooldown

    def record_failure(self) -> None:
        self.consecutive_failures += 1
        self.last_failure_at = datetime.now(timezone.utc)
        idx = min(self.consecutive_failures - 1, len(_COOLDOWN_SCHEDULE) - 1)
        self.cooldown_until = self.last_failure_at + _COOLDOWN_SCHEDULE[idx]
        self.dirty = True

    def record_success(self) -> None:
        if self.consecutive_failures > 0:
            self.consecutive_failures = 0
            self.last_failure_at = None  # Clear stale timestamp
            self.cooldown_until = None
            self.dirty = True


def load_breaker_state(db: Session, user_id: str) -> dict[str, BreakerRecord]:
    """Load breaker state for a user. 1 SELECT per turn.

    Returns {tool_name: BreakerRecord}.
    """
    rows = db.query(ToolBreakerState).filter_by(user_id=user_id).all()
    return {
        r.tool_name: BreakerRecord(
            user_id=r.user_id,
            tool_name=r.tool_name,
            consecutive_failures=r.consecutive_failures,
            last_failure_at=r.last_failure_at,
            cooldown_until=r.cooldown_until,
            dirty=False,
        )
        for r in rows
    }


def flush_breaker_state(db: Session, records: dict[str, BreakerRecord]) -> int:
    """Persist all dirty records in one batch. 1 transaction per turn.

    Uses merge() to avoid per-record SELECT — the DB handles INSERT-or-UPDATE
    in a single round-trip per record.

    Returns number of records written.
    """
    dirty = [rec for rec in records.values() if rec.dirty]
    if not dirty:
        return 0
    now = datetime.now(timezone.utc)
    for rec in dirty:
        db.merge(ToolBreakerState(
            user_id=rec.user_id,
            tool_name=rec.tool_name,
            consecutive_failures=rec.consecutive_failures,
            last_failure_at=rec.last_failure_at,
            cooldown_until=rec.cooldown_until,
            updated_at=now,
        ))
    db.commit()
    # Clear dirty only AFTER successful commit — if commit fails, retry will re-write
    for rec in dirty:
        rec.dirty = False
    return len(dirty)
