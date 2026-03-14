"""Integration tests for three-module closedloop gap fixes.

P1: Selector Pipeline — execution metrics backfill to skill_selection_events
P2: Memory Governance — structured governance outputs (compress, contradiction, quarantine)
P3: Skill-as-Package — rollback lifecycle
"""

import os
import json
import pytest
from datetime import datetime, timedelta
from uuid import uuid4
from sqlalchemy import text
from sqlalchemy.orm import Session

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")


def _session_local():
    from api import database

    return database.SessionLocal


@pytest.fixture
def db_session():
    db = _session_local()()
    yield db
    db.rollback()
    db.close()


# ── P1: Selector Pipeline Feedback Closedloop ──────────────────────


class TestGovernanceStructuredOutputs:
    """Verify governance actions produce queryable events."""

    def test_quarantine_writes_governance_event(self, db_session):
        """Quarantine action writes governance_quarantine event."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from api.models import KnowledgeEntry
        from uuid_utils import uuid7

        engine = MemoryGovernanceEngine(lambda: db_session)

        eid = str(uuid7())
        db_session.add(
            KnowledgeEntry(
                entry_id=eid,
                user_id="test",
                category="test",
                key_name="k1",
                value="v1",
                trust_tier="T4",
                confidence=0.1,
                initial_confidence=0.4,
                created_at=datetime.now(),
            )
        )
        db_session.commit()

        engine._quarantine_low_confidence(threshold=0.3)

        row = db_session.execute(
            text("""
            SELECT content FROM agent_events
            WHERE event_type = 'governance_quarantine'
            ORDER BY created_at DESC LIMIT 1
        """)
        ).fetchone()
        assert row is not None
        data = json.loads(row[0])
        assert eid in data["entry_ids"]

    def test_contradiction_scan_writes_event(self, db_session):
        """Contradiction scan writes contradiction_detected event."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from api.models import KnowledgeEntry
        from uuid_utils import uuid7

        engine = MemoryGovernanceEngine(lambda: db_session)
        cat = f"cat_{uuid4().hex}"

        for val in ["value_A", "value_B"]:
            db_session.add(
                KnowledgeEntry(
                    entry_id=str(uuid7()),
                    user_id="test",
                    category=cat,
                    key_name="same_key",
                    value=val,
                    trust_tier="T3",
                    confidence=0.8,
                    initial_confidence=0.8,
                    created_at=datetime.now(),
                )
            )
        db_session.commit()

        count = engine._scan_contradictions()
        assert count >= 1

        row = db_session.execute(
            text("""
            SELECT content FROM agent_events
            WHERE event_type = 'contradiction_detected'
            AND content LIKE :pat
            ORDER BY created_at DESC LIMIT 1
        """),
            {"pat": f"%{cat}%"},
        ).fetchone()
        assert row is not None
        data = json.loads(row[0])
        assert data["category"] == cat
        assert len(data["entry_ids"]) == 2


# ── P3: Skill-as-Package Rollback ──────────────────────────────────
