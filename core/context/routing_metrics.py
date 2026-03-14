"""Routing system metrics — active request tracking + adaptive threshold.

Design doc: docs/design/token-efficient-llm-routing.md (v3) §Adaptive Confidence Threshold
"""

from __future__ import annotations

import logging
import os
import threading
import time
from contextlib import contextmanager
from datetime import datetime, timezone

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Active request counter (fast atomic read, no Prometheus overhead)
# ---------------------------------------------------------------------------

_active_requests = 0
_active_lock = threading.Lock()
_CAPACITY = int(os.environ.get("ROUTING_CAPACITY", "20"))


@contextmanager
def active_request_context():
    """Increment active request count for duration of context."""
    global _active_requests
    with _active_lock:
        _active_requests += 1
    try:
        yield
    finally:
        with _active_lock:
            _active_requests = max(_active_requests - 1, 0)


def current_load() -> float:
    """Return current load as ratio (0.0-1.0+)."""
    return _active_requests / max(_CAPACITY, 1)


# ---------------------------------------------------------------------------
# Monthly budget remaining (TTL-cached DB query)
# ---------------------------------------------------------------------------

_MONTHLY_BUDGET = float(os.environ.get("MONTHLY_LLM_BUDGET_USD", "100.0"))
_budget_cache: tuple[float, float] | None = None  # (remaining_ratio, expires_at)
_BUDGET_TTL = 60.0  # seconds


def monthly_budget_remaining(db_factory=None) -> float:
    """Return ratio of monthly budget remaining (0.0-1.0). Cached 60s."""
    global _budget_cache
    now = time.monotonic()
    if _budget_cache and _budget_cache[1] > now:
        return _budget_cache[0]

    if _MONTHLY_BUDGET <= 0:
        _budget_cache = (1.0, now + _BUDGET_TTL)
        return 1.0

    spent = 0.0
    if db_factory:
        try:
            from sqlalchemy import text

            first_of_month = datetime.now(timezone.utc).replace(
                day=1, hour=0, minute=0, second=0, microsecond=0
            )
            db = db_factory()
            try:
                row = db.execute(
                    text(
                        "SELECT COALESCE(SUM(cost_usd), 0) FROM eval_llm_call_logs WHERE created_at >= :since"
                    ),
                    {"since": first_of_month},
                ).fetchone()
                spent = float(row[0]) if row else 0.0
            finally:
                db.close()
        except Exception as e:
            logger.debug("Monthly budget query failed: %s", e)

    remaining = max(1.0 - spent / _MONTHLY_BUDGET, 0.0)
    _budget_cache = (remaining, now + _BUDGET_TTL)
    return remaining


# ---------------------------------------------------------------------------
# Adaptive threshold
# ---------------------------------------------------------------------------


def adaptive_threshold(base: float = 0.85, db_factory=None) -> float:
    """Adjust routing confidence threshold based on system state.

    Normal:     0.85 (balanced)
    High load:  0.75 (route more aggressively)
    Low budget: 0.92 (prefer fallback over misclassification)
    """
    t = base
    if current_load() > 0.8:
        t -= 0.10
    if monthly_budget_remaining(db_factory) < 0.2:
        t += 0.07
    return max(0.70, min(t, 0.95))


def reset_for_testing() -> None:
    """Reset module state for test isolation."""
    global _active_requests, _budget_cache
    _active_requests = 0
    _budget_cache = None
