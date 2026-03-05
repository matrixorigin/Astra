"""Integration tests for skills/knowledge/api.py — real DB, no mocks."""

import json

import pytest
from uuid_utils import uuid7

from api.database import get_db_session
from api.models import KnowledgeEntry
from skills.knowledge.api import (
    KnowledgeExtractor,
    _normalize_value,
    add_relation,
    expand_with_graph,
    get_neighbors,
    update_access_tracking,
)


@pytest.fixture
def db():
    return next(get_db_session())


@pytest.fixture
def user_id():
    return f"test-know-{uuid7()}"


def _insert_entry(db, user_id, *, key_name="lang", value="python", confidence=0.8, trust_tier="T3"):
    eid = str(uuid7())
    e = KnowledgeEntry(
        entry_id=eid,
        user_id=user_id,
        category="user_preference",
        key_name=key_name,
        value=value,
        extraction_method="test",
        trust_tier=trust_tier,
        confidence=confidence,
        initial_confidence=confidence,
        access_count=0,
        version=1,
    )
    db.add(e)
    db.commit()
    return eid


# ── update_access_tracking ────────────────────────────────────────────────────

class TestAccessTracking:
    def test_bumps_access_count(self, db, user_id):
        import threading
        from api.database import SessionLocal
        eid = _insert_entry(db, user_id)
        done = threading.Event()
        update_access_tracking(SessionLocal, [eid], _done=done)
        done.wait(timeout=5)
        db.expire_all()
        row = db.get(KnowledgeEntry, eid)
        assert row.access_count == 1
        assert row.last_accessed_at is not None

    def test_multiple_bumps(self, db, user_id):
        import threading
        from api.database import SessionLocal
        eid = _insert_entry(db, user_id)
        d1 = threading.Event()
        update_access_tracking(SessionLocal, [eid], _done=d1)
        d1.wait(timeout=5)
        d2 = threading.Event()
        update_access_tracking(SessionLocal, [eid], _done=d2)
        d2.wait(timeout=5)
        db.expire_all()
        row = db.get(KnowledgeEntry, eid)
        assert row.access_count == 2

    def test_empty_list_noop(self, db):
        update_access_tracking(lambda: db, [])  # should not raise


# ── Knowledge Graph ───────────────────────────────────────────────────────────

class TestKnowledgeGraph:
    def test_add_and_get_neighbors(self, db, user_id):
        e1 = _insert_entry(db, user_id, key_name="k1", value="v1")
        e2 = _insert_entry(db, user_id, key_name="k2", value="v2")

        rid = add_relation(db, e1, "related_to", e2, weight=0.9)
        assert rid is not None

        neighbors = get_neighbors(db, e1, direction="outgoing")
        assert any(n["neighbor_id"] == e2 for n in neighbors)

        neighbors_in = get_neighbors(db, e2, direction="incoming")
        assert any(n["neighbor_id"] == e1 for n in neighbors_in)

    def test_add_relation_idempotent(self, db, user_id):
        e1 = _insert_entry(db, user_id, key_name="k3", value="v3")
        e2 = _insert_entry(db, user_id, key_name="k4", value="v4")

        add_relation(db, e1, "depends_on", e2, weight=0.5)
        add_relation(db, e1, "depends_on", e2, weight=0.8)  # upsert

        neighbors = get_neighbors(db, e1, direction="outgoing")
        deps = [n for n in neighbors if n["neighbor_id"] == e2 and n["predicate"] == "depends_on"]
        assert len(deps) == 1
        assert deps[0]["weight"] == 0.8  # updated

    def test_expand_with_graph(self, db, user_id):
        e1 = _insert_entry(db, user_id, key_name="k5", value="v5")
        e2 = _insert_entry(db, user_id, key_name="k6", value="v6")
        e3 = _insert_entry(db, user_id, key_name="k7", value="v7")

        add_relation(db, e1, "related_to", e2)
        add_relation(db, e2, "related_to", e3)

        expanded = expand_with_graph(db, [e1])
        assert e2 in expanded
        # e3 is 2-hop from e1, should NOT appear
        assert e3 not in expanded

    def test_expand_empty_input(self, db):
        assert expand_with_graph(db, []) == []

    def test_get_neighbors_predicate_filter(self, db, user_id):
        e1 = _insert_entry(db, user_id, key_name="k8", value="v8")
        e2 = _insert_entry(db, user_id, key_name="k9", value="v9")
        e3 = _insert_entry(db, user_id, key_name="k10", value="v10")

        add_relation(db, e1, "related_to", e2)
        add_relation(db, e1, "contradicts", e3)

        neighbors = get_neighbors(db, e1, predicates=["contradicts"], direction="outgoing")
        assert len(neighbors) == 1
        assert neighbors[0]["neighbor_id"] == e3


# ── KnowledgeExtractor (batch store, decay, quarantine) ──────────────────────

class TestKnowledgeExtractor:
    def test_batch_store_creates_entry(self, db, user_id):
        ext = KnowledgeExtractor(db)
        result = ext._batch_store_knowledge([{
            "user_id": user_id,
            "category": "user_preference",
            "key_name": "editor",
            "value": "vim",
            "source_event_ids": ["e1"],
            "extraction_method": "test",
            "trust_tier": "T3",
            "confidence": 0.7,
        }])
        assert len(result) == 1
        assert result[0]["action"] == "created"

        row = db.get(KnowledgeEntry, result[0]["entry_id"])
        assert row.value == "vim"
        assert row.confidence == 0.7

    def test_batch_store_reinforces_same_value(self, db, user_id):
        _insert_entry(db, user_id, key_name="editor", value="vim", confidence=0.7)

        ext = KnowledgeExtractor(db)
        result = ext._batch_store_knowledge([{
            "user_id": user_id,
            "category": "user_preference",
            "key_name": "editor",
            "value": "vim",
            "source_event_ids": ["e2"],
            "extraction_method": "test",
            "trust_tier": "T3",
            "confidence": 0.7,
        }])
        assert result[0]["action"] == "updated"
        assert result[0]["confidence"] == pytest.approx(0.8)  # 0.7 + 0.1

    def test_batch_store_synonym_reinforces(self, db, user_id):
        """'JS' and 'javascript' should reinforce, not contradict."""
        _insert_entry(db, user_id, key_name="lang", value="JS", confidence=0.7)

        ext = KnowledgeExtractor(db)
        result = ext._batch_store_knowledge([{
            "user_id": user_id,
            "category": "user_preference",
            "key_name": "lang",
            "value": "javascript",
            "source_event_ids": ["e3"],
            "extraction_method": "test",
            "trust_tier": "T3",
            "confidence": 0.7,
        }])
        assert result[0]["action"] == "updated"

    def test_batch_store_contradiction(self, db, user_id):
        eid = _insert_entry(db, user_id, key_name="framework", value="react", confidence=0.8)

        ext = KnowledgeExtractor(db)
        result = ext._batch_store_knowledge([{
            "user_id": user_id,
            "category": "user_preference",
            "key_name": "framework",
            "value": "vue",
            "source_event_ids": ["e4"],
            "extraction_method": "test",
            "trust_tier": "T3",
            "confidence": 0.7,
        }])
        assert result[0]["action"] == "contradiction"

        old = db.get(KnowledgeEntry, eid)
        assert old.confidence == 0.5  # 0.8 - 0.3
        assert old.superseded_by == result[0]["entry_id"]

    def test_decay_confidence(self, db, user_id):
        from datetime import datetime, timedelta
        from sqlalchemy import text as sa_text

        eid = _insert_entry(db, user_id, key_name="old_fact", value="stale", confidence=0.9, trust_tier="T3")
        # Backdate last_validated_at to 90 days ago
        db.execute(
            sa_text("UPDATE sk_knowledge_entries SET last_validated_at = :ts WHERE entry_id = :eid"),
            {"ts": datetime.now() - timedelta(days=90), "eid": eid},
        )
        db.commit()

        ext = KnowledgeExtractor(db)
        count = ext.decay_confidence(user_id)
        assert count >= 1

        row = db.get(KnowledgeEntry, eid)
        assert row.confidence < 0.9  # decayed

    def test_quarantine_low_confidence(self, db, user_id):
        eid = _insert_entry(db, user_id, key_name="weak", value="maybe", confidence=0.1)

        ext = KnowledgeExtractor(db)
        count = ext.quarantine_low_confidence(user_id, threshold=0.3)
        assert count >= 1

        row = db.get(KnowledgeEntry, eid)
        assert row.confidence == 0

    def test_quarantine_skips_already_zero(self, db, user_id):
        _insert_entry(db, user_id, key_name="dead", value="gone", confidence=0.0)

        ext = KnowledgeExtractor(db)
        count = ext.quarantine_low_confidence(user_id, threshold=0.3)
        assert count == 0  # already at 0, should not re-quarantine
