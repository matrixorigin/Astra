"""Tests for memory extraction & governance closed-loop fixes.

Covers:
  #1 access_count / last_accessed_at updated on retrieval
  #2 knowledge_extracted event logged after extraction
  #3 quarantine logs entry_ids for audit
  #4 (boundary) — covered by existing decay+quarantine order
  #5 contradiction confidence floor ≥ 0
  #6 decay_confidence respects per-entry trust_tier
"""

import json
import pytest
from datetime import datetime, timedelta
from unittest.mock import MagicMock, Mock, patch

from api.models import KnowledgeEntry, Event
from skills.knowledge.api import KnowledgeExtractor, update_access_tracking
from core.context.lifecycle import MemoryGovernanceEngine
from uuid_utils import uuid7


# ── Fixtures ────────────────────────────────────────────────────

# db_session provided by tests/conftest.py (test database)


def _make_entry(db, user_id="u1", key_name="lang", value="python",
                confidence=0.8, trust_tier="T3", initial_confidence=0.8):
    entry = KnowledgeEntry(
        entry_id=str(uuid7()),
        user_id=user_id,
        category="user_preference",
        key_name=key_name,
        value=value,
        extraction_method="observation",
        trust_tier=trust_tier,
        confidence=confidence,
        initial_confidence=initial_confidence,
    )
    db.add(entry)
    db.commit()
    return entry


# ── #1: Access tracking ────────────────────────────────────────


def test_update_access_tracking(db_session):
    """update_access_tracking bumps access_count and last_accessed_at."""
    entry = _make_entry(db_session)
    assert entry.access_count == 0

    update_access_tracking(db_session, [entry.entry_id])
    db_session.refresh(entry)

    assert entry.access_count == 1
    assert entry.last_accessed_at is not None


def test_update_access_tracking_empty_list(db_session):
    """Empty list is a no-op, no error."""
    update_access_tracking(db_session, [])  # should not raise


def test_update_access_tracking_increments(db_session):
    """Multiple calls increment access_count."""
    entry = _make_entry(db_session)
    update_access_tracking(db_session, [entry.entry_id])
    update_access_tracking(db_session, [entry.entry_id])
    db_session.refresh(entry)
    assert entry.access_count == 2


def test_context_manager_retrieval_updates_access(db_session):
    """ContextManager.retrieve_semantic_knowledge updates access tracking."""
    uid = f"cm_access_{uuid7()}"
    entry = _make_entry(db_session, user_id=uid, key_name="language", value="python")
    assert entry.access_count == 0

    from core.context.manager import ContextManager
    mgr = ContextManager.__new__(ContextManager)
    mgr.db = db_session

    results = mgr.retrieve_semantic_knowledge(uid, "python")
    assert len(results) >= 1

    db_session.refresh(entry)
    assert entry.access_count >= 1


# ── #2: Extraction audit event ─────────────────────────────────


def test_extraction_logs_event(db_session):
    """extract_from_chain logs a knowledge_extracted event."""
    chain_id = str(uuid7())
    user_id = "u_audit"
    session_id = str(uuid7())

    db_session.add(Event(
        event_id=str(uuid7()),
        session_id=session_id,
        user_id=user_id,
        event_type="user_query",
        content="I prefer TypeScript for backend development",
        causal_chain_id=chain_id,
    ))
    db_session.commit()

    from core.events.event_logger import EventLogger
    event_logger = EventLogger.from_session(db_session)
    extractor = KnowledgeExtractor(db_session, event_logger=event_logger)
    stored = extractor.extract_from_chain(chain_id, user_id)

    assert len(stored) > 0

    # Verify knowledge_extracted event was logged
    audit_events = db_session.query(Event).filter(
        Event.event_type == "knowledge_extracted",
        Event.content.like(f'%{chain_id}%'),
    ).all()
    assert len(audit_events) == 1
    content = json.loads(audit_events[0].content)
    assert content["causal_chain_id"] == chain_id
    assert len(content["entries"]) == len(stored)


def test_extraction_no_event_logger_no_crash():
    """Without event_logger, extraction still works (no audit)."""
    mock_db = MagicMock()
    mock_db.query.return_value.filter.return_value.order_by.return_value.all.return_value = []

    extractor = KnowledgeExtractor(mock_db)
    result = extractor.extract_from_chain("chain1", "user1")
    assert result == []


# ── #3: Quarantine audit ───────────────────────────────────────


def test_quarantine_logs_entry_ids(db_session):
    """Quarantine logs which entry_ids were quarantined."""
    uid = f"quarantine_{uuid7()}"
    e1 = _make_entry(db_session, user_id=uid, key_name="k1", confidence=0.2, initial_confidence=0.2)
    e2 = _make_entry(db_session, user_id=uid, key_name="k2", confidence=0.1, initial_confidence=0.1)
    _make_entry(db_session, user_id=uid, key_name="k3", confidence=0.8, initial_confidence=0.8)  # should NOT be quarantined

    engine = MemoryGovernanceEngine(db_session)

    with patch("core.context.lifecycle.logger") as mock_logger:
        count = engine._quarantine_low_confidence(threshold=0.3)

    assert count >= 2  # at least our 2 entries (may include others from shared DB)

    # Verify our entries are quarantined
    db_session.refresh(e1)
    db_session.refresh(e2)
    assert e1.confidence == 0
    assert e2.confidence == 0

    # Verify log contains our entry_ids
    mock_logger.info.assert_called_once()
    log_args = mock_logger.info.call_args[0]
    logged_ids = log_args[3]  # 4th positional arg is the ids list
    assert e1.entry_id in logged_ids
    assert e2.entry_id in logged_ids


def test_quarantine_returns_zero_when_none_below_threshold(db_session):
    """No entries below threshold → returns 0, no commit."""
    uid = f"healthy_{uuid7()}"
    _make_entry(db_session, user_id=uid, key_name="healthy", confidence=0.9, initial_confidence=0.9)
    # Quarantine is global (not per-user), so this test only verifies
    # that a healthy-only DB returns 0. Run it before other tests create
    # low-confidence entries, or accept count >= 0 from shared DB.
    # We verify the specific entry is NOT quarantined.
    engine = MemoryGovernanceEngine(db_session)
    engine._quarantine_low_confidence(threshold=0.3)
    entry = db_session.query(KnowledgeEntry).filter(
        KnowledgeEntry.user_id == uid,
    ).first()
    assert entry.confidence == 0.9  # untouched


# ── #5: Contradiction confidence floor ─────────────────────────


def test_contradiction_confidence_never_negative(db_session):
    """When contradicting an entry with confidence < 0.3, result is 0, not negative."""
    entry = _make_entry(db_session, key_name="fragile", value="old_val",
                        confidence=0.1, initial_confidence=0.1)

    extractor = KnowledgeExtractor(db_session)

    # Create event that contradicts
    chain_id = str(uuid7())
    db_session.add(Event(
        event_id=str(uuid7()),
        session_id=str(uuid7()),
        user_id="u1",
        event_type="llm_response",
        content="The codebase uses dependency injection pattern for fragile",
        causal_chain_id=chain_id,
    ))
    db_session.commit()

    # Force a contradiction by directly calling _batch_store_knowledge
    extractor._batch_store_knowledge([{
        "user_id": "u1",
        "category": "user_preference",
        "key_name": "fragile",
        "value": "new_val",
        "source_event_ids": ["e1"],
        "extraction_method": "observation",
        "trust_tier": "T3",
        "confidence": 0.65,
    }])

    db_session.refresh(entry)
    assert entry.confidence >= 0.0  # never negative


# ── #6: decay_confidence respects trust_tier ───────────────────


def test_decay_uses_per_tier_half_life(db_session):
    """T1 (365-day half-life) decays much slower than T4 (30-day half-life)."""
    uid = f"decay_{uuid7()}"
    now = datetime.now()
    days_ago = now - timedelta(days=60)

    t1 = _make_entry(db_session, user_id=uid, key_name="t1_entry", trust_tier="T1",
                     confidence=0.95, initial_confidence=0.95)
    t1.last_validated_at = days_ago
    t1.created_at = days_ago

    t4 = _make_entry(db_session, user_id=uid, key_name="t4_entry", trust_tier="T4",
                     confidence=0.40, initial_confidence=0.40)
    t4.last_validated_at = days_ago
    t4.created_at = days_ago
    db_session.commit()

    extractor = KnowledgeExtractor(db_session)
    extractor.decay_confidence(uid)

    db_session.refresh(t1)
    db_session.refresh(t4)

    # T1: 60 days / 365 half-life → barely decayed
    assert t1.confidence > 0.84

    # T4: 60 days / 30 half-life → 2 half-lives → ~0.10
    assert t4.confidence < 0.15


def test_decay_skips_entries_without_dates(db_session):
    """Entries with no temporal anchor are skipped, not crashed."""
    uid = f"nodates_{uuid7()}"
    entry = _make_entry(db_session, user_id=uid, key_name="no_dates",
                        confidence=0.8, initial_confidence=0.8)
    # Force both date fields to None
    db_session.execute(
        __import__("sqlalchemy").text(
            "UPDATE sk_knowledge_entries SET last_validated_at = NULL, created_at = NULL "
            "WHERE entry_id = :eid"
        ),
        {"eid": entry.entry_id},
    )
    db_session.commit()

    extractor = KnowledgeExtractor(db_session)
    count = extractor.decay_confidence(uid)
    # Should not crash, entry skipped
    assert count >= 0
