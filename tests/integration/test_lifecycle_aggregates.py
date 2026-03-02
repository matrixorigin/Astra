"""Integration tests for MemoryGovernanceEngine aggregate queries against real DB.

Validates that _archive_closed_notes, _generate_health_reports,
_get_user_memory_stats, and governance_stats produce correct SQL.
"""

import pytest
from uuid_utils import uuid7

from api.database import get_db_session
from api.models import KnowledgeEntry, AgentScratchpad
from core.context.lifecycle import MemoryGovernanceEngine


@pytest.fixture
def db():
    gen = get_db_session()
    session = next(gen)
    yield session
    try:
        next(gen)
    except StopIteration:
        pass


@pytest.fixture
def engine(db):
    return MemoryGovernanceEngine(lambda: db)


@pytest.fixture
def user_id():
    return f"test-lc-{uuid7().hex[:8]}"


@pytest.fixture
def _seed_knowledge(db, user_id):
    """Insert 3 knowledge entries: 2 healthy, 1 low-confidence."""
    ids = []
    for conf, tier in [(0.9, "T1"), (0.7, "T2"), (0.1, "T3")]:
        eid = str(uuid7())
        ids.append(eid)
        db.add(KnowledgeEntry(
            entry_id=eid, user_id=user_id, category="test",
            key_name=f"k_{eid[:6]}", value="v",
            extraction_method="test", trust_tier=tier,
            confidence=conf, initial_confidence=conf,
            access_count=0, version=1,
        ))
    db.commit()
    yield ids
    # cleanup
    from sqlalchemy import text
    db.execute(text("DELETE FROM sk_knowledge_entries WHERE entry_id IN :ids"),
               {"ids": tuple(ids)})
    db.commit()


@pytest.fixture
def _seed_scratchpads(db, user_id):
    """Insert 2 completed + 1 active scratchpad notes."""
    ids = []
    for status in ("completed", "completed", "active"):
        nid = str(uuid7())
        ids.append(nid)
        db.add(AgentScratchpad(
            note_id=nid, session_id=str(uuid7()), user_id=user_id,
            note_type="plan", content="test", status=status,
        ))
    db.commit()
    yield ids
    from sqlalchemy import text
    db.execute(text("DELETE FROM agent_scratchpads WHERE note_id IN :ids"),
               {"ids": tuple(ids)})
    db.commit()


class TestLifecycleAggregates:
    """Verify aggregate SQL runs correctly against real MatrixOne."""

    def test_archive_closed_notes(self, engine, db, _seed_scratchpads):
        count = engine._archive_closed_notes()
        assert count == 2  # exactly our 2 completed notes
        # Verify status actually changed
        archived = db.query(AgentScratchpad).filter(
            AgentScratchpad.note_id.in_(_seed_scratchpads),
            AgentScratchpad.status == "archived",
        ).count()
        assert archived == 2
        # Active note untouched
        active = db.query(AgentScratchpad).filter(
            AgentScratchpad.note_id.in_(_seed_scratchpads),
            AgentScratchpad.status == "active",
        ).count()
        assert active == 1

    def test_generate_health_reports(self, engine, _seed_knowledge):
        reports = engine._generate_health_reports()
        assert reports >= 1  # at least 1 user with entries

    def test_get_user_memory_stats(self, engine, user_id, _seed_knowledge):
        stats = engine._get_user_memory_stats(user_id)
        assert stats["total_entries"] == 3
        assert 0.5 < stats["avg_confidence"] < 0.7  # avg(0.9, 0.7, 0.1)
        assert stats["low_confidence"] == 1  # only 0.1 < 0.3

    def test_governance_stats(self, engine, user_id, _seed_knowledge):
        stats = engine.governance_stats()
        assert stats["total_entries"] >= 3
        assert "avg_confidence" in stats
        assert "tier_distribution" in stats
        assert isinstance(stats["tier_distribution"], dict)
        assert stats["quarantined"] >= 1  # at least our 0.1 entry
        assert stats["contradictions"] >= 0  # read-only count, no side effects
        # Verify our tiers are present
        for tier in ("T1", "T2", "T3"):
            assert tier in stats["tier_distribution"]

    def test_empty_stats(self, db):
        """governance_stats on user with no data returns total_entries=0."""
        eng = MemoryGovernanceEngine(lambda: db)
        # This just verifies the SQL doesn't crash; other tests may have data
        stats = eng._get_user_memory_stats(f"nonexistent-{uuid7().hex}")
        assert stats["total_entries"] == 0
