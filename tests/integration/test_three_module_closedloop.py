"""Integration tests for three-module closedloop gap fixes.

P1: Selector Pipeline — execution metrics backfill to skill_selection_events
P2: Memory Governance — structured governance outputs (compress, contradiction, quarantine)
P3: Skill-as-Package — rollback lifecycle
"""

import os
import json
import pytest
from datetime import datetime, timedelta
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


class TestSelectorFeedbackClosedloop:
    """Verify execution metrics flow back to skill_selection_events."""

    def test_backfill_updates_selection_event(self, db_session):
        """After execution, skill_selection_events row has time/cost/success."""
        from core.agent.executor import AgentExecutor
        from core.skills.registry import SkillRegistry

        event_id = f"sel_{os.urandom(4).hex()}"
        db_session.execute(text("""
            INSERT INTO skill_selection_events (event_id, session_id, user_query, selected_skills, selection_method, created_at)
            VALUES (:eid, 'sess1', 'test query', '["test_skill"]', 'keyword', NOW())
        """), {"eid": event_id})
        db_session.commit()

        executor = AgentExecutor(lambda: db_session, SkillRegistry(lambda: db_session))
        executor._backfill_selection_event(event_id, 150, 0.005, True)

        row = db_session.execute(text(
            "SELECT execution_time_ms, execution_cost, execution_success FROM skill_selection_events WHERE event_id = :eid"
        ), {"eid": event_id}).fetchone()
        assert row is not None
        assert row[0] == 150
        assert abs(row[1] - 0.005) < 0.001
        assert row[2] == 1

    def test_backfill_records_failure(self, db_session):
        """Failed execution sets execution_success=0."""
        from core.agent.executor import AgentExecutor
        from core.skills.registry import SkillRegistry

        event_id = f"sel_{os.urandom(4).hex()}"
        db_session.execute(text("""
            INSERT INTO skill_selection_events (event_id, session_id, user_query, selected_skills, selection_method, created_at)
            VALUES (:eid, 'sess1', 'test', '["s"]', 'keyword', NOW())
        """), {"eid": event_id})
        db_session.commit()

        executor = AgentExecutor(lambda: db_session, SkillRegistry(lambda: db_session))
        executor._backfill_selection_event(event_id, 5000, 0.0, False)

        row = db_session.execute(text(
            "SELECT execution_success FROM skill_selection_events WHERE event_id = :eid"
        ), {"eid": event_id}).fetchone()
        assert row[0] == 0


# ── P2: Memory Governance Structured Outputs ───────────────────────


class TestGovernanceStructuredOutputs:
    """Verify governance actions produce queryable events."""

    def test_quarantine_writes_governance_event(self, db_session):
        """Quarantine action writes governance_quarantine event."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from api.models import KnowledgeEntry
        from uuid_utils import uuid7

        engine = MemoryGovernanceEngine(lambda: db_session)

        eid = str(uuid7())
        db_session.add(KnowledgeEntry(
            entry_id=eid, user_id="test", category="test",
            key_name="k1", value="v1", trust_tier="T4",
            confidence=0.1, initial_confidence=0.4,
            created_at=datetime.now(),
        ))
        db_session.commit()

        engine._quarantine_low_confidence(threshold=0.3)

        row = db_session.execute(text("""
            SELECT content FROM conversation_events
            WHERE event_type = 'governance_quarantine'
            ORDER BY created_at DESC LIMIT 1
        """)).fetchone()
        assert row is not None
        data = json.loads(row[0])
        assert eid in data["entry_ids"]

    def test_contradiction_scan_writes_event(self, db_session):
        """Contradiction scan writes contradiction_detected event."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from api.models import KnowledgeEntry
        from uuid_utils import uuid7

        engine = MemoryGovernanceEngine(lambda: db_session)
        cat = f"cat_{os.urandom(4).hex()}"

        for val in ["value_A", "value_B"]:
            db_session.add(KnowledgeEntry(
                entry_id=str(uuid7()), user_id="test", category=cat,
                key_name="same_key", value=val, trust_tier="T3",
                confidence=0.8, initial_confidence=0.8,
                created_at=datetime.now(),
            ))
        db_session.commit()

        count = engine._scan_contradictions()
        assert count >= 1

        row = db_session.execute(text("""
            SELECT content FROM conversation_events
            WHERE event_type = 'contradiction_detected'
            AND content LIKE :pat
            ORDER BY created_at DESC LIMIT 1
        """), {"pat": f"%{cat}%"}).fetchone()
        assert row is not None
        data = json.loads(row[0])
        assert data["category"] == cat
        assert len(data["entry_ids"]) == 2

    def test_compress_episodic_writes_summary(self, db_session):
        """Episodic compression writes session_summary and marks originals."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from uuid_utils import uuid7

        engine = MemoryGovernanceEngine(lambda: db_session)
        sid = f"sess_{os.urandom(4).hex()}"
        old_ts = datetime.now() - timedelta(days=100)

        for i in range(3):
            eid = str(uuid7())
            db_session.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, agent_version,
                     event_type, content, causal_chain_id, created_at)
                VALUES (:eid, :sid, 'test', 'test', '1.0', :etype, :content, :cid, :ts)
            """), {
                "eid": eid, "sid": sid, "cid": eid,
                "etype": "user_query" if i % 2 == 0 else "llm_response",
                "content": f"message {i}", "ts": old_ts,
            })
        db_session.commit()

        count = engine._compress_episodic_events(ttl_days=90)
        assert count >= 3

        summary = db_session.execute(text("""
            SELECT content FROM conversation_events
            WHERE session_id = :sid AND event_type = 'session_summary'
        """), {"sid": sid}).fetchone()
        assert summary is not None

        compressed = db_session.execute(text("""
            SELECT COUNT(*) FROM conversation_events
            WHERE session_id = :sid AND event_type = 'compressed'
        """), {"sid": sid}).fetchone()
        assert compressed[0] == 3


# ── P3: Skill-as-Package Rollback ──────────────────────────────────


class TestSkillRollback:
    """Verify upgrade→rollback lifecycle."""

    def _create_skill_def(self, db, name, version="1.0"):
        from api.models import SkillDefinition
        from uuid_utils import uuid7
        sd = SkillDefinition(
            skill_id=str(uuid7()), name=name, version=version,
            is_active=1, is_public=1, manifest={},
        )
        db.add(sd)
        db.commit()
        return sd

    def test_upgrade_preserves_previous_version(self, db_session, db_factory):
        """Upgrade stores previous_version for rollback."""
        from core.skills.skill_manager import SkillManager
        from core.skills.credential_manager import CredentialManager

        uid = f"u_{os.urandom(4).hex()}"
        skill = f"trb_{os.urandom(4).hex()}"
        self._create_skill_def(db_session, skill, "1.0")

        mgr = SkillManager(db_factory, CredentialManager("test-secret-key-1234567890123456"))
        inst = mgr.install(uid, skill)
        assert inst.skill_version == "1.0"

        db_session.execute(text(
            "UPDATE skill_definitions SET version = '2.0' WHERE name = :n"
        ), {"n": skill})
        db_session.commit()

        inst = mgr.upgrade(uid, skill)
        assert inst.skill_version == "2.0"
        assert inst.previous_version == "1.0"

    def test_rollback_restores_previous_version(self, db_session, db_factory):
        """Rollback swaps version back."""
        from core.skills.skill_manager import SkillManager
        from core.skills.credential_manager import CredentialManager

        uid = f"u_{os.urandom(4).hex()}"
        skill = f"rb_{os.urandom(4).hex()}"
        self._create_skill_def(db_session, skill, "1.0")

        mgr = SkillManager(db_factory, CredentialManager("test-secret-key-1234567890123456"))
        mgr.install(uid, skill)

        db_session.execute(text(
            "UPDATE skill_definitions SET version = '2.0' WHERE name = :n"
        ), {"n": skill})
        db_session.commit()
        mgr.upgrade(uid, skill)

        inst = mgr.rollback(uid, skill)
        assert inst.skill_version == "1.0"
        assert inst.previous_version == "2.0"

    def test_rollback_no_previous_raises(self, db_session, db_factory):
        """Rollback without previous version raises error."""
        from core.skills.skill_manager import SkillManager, SkillNotInstalledError
        from core.skills.credential_manager import CredentialManager

        uid = f"u_{os.urandom(4).hex()}"
        skill = f"norb_{os.urandom(4).hex()}"
        self._create_skill_def(db_session, skill, "1.0")

        mgr = SkillManager(db_factory, CredentialManager("test-secret-key-1234567890123456"))
        mgr.install(uid, skill)

        with pytest.raises(SkillNotInstalledError, match="no previous version"):
            mgr.rollback(uid, skill)
