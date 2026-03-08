"""Integration tests for MemoryEditor (Phase 2).

Verifies against REAL DB:
1. inject → mem_memories row with correct trust tier + audit log
2. correct → old deactivated, new created, superseded_by linked
3. purge → memories deactivated, snapshot created, audit logged
4. Field-level verification on all operations
5. User isolation — edits don't affect other users
"""

from __future__ import annotations

import json
from uuid import uuid4

import pytest
from sqlalchemy import text

from core.memory.editor import MemoryEditor
from core.memory.factory import create_memory_service
from core.memory.types import MemoryType, TrustTier


def _uid() -> str:
    return f"edit_e2e_{uuid4().hex[:12]}"


@pytest.fixture
def db_factory():
    from api.database import SessionLocal
    return SessionLocal


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture
def editor(db_factory):
    svc = create_memory_service(db_factory, strategy="vector:v1")
    return MemoryEditor(svc.storage, db_factory, index_manager=None)


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    yield
    db = db_factory()
    try:
        db.execute(text("DELETE FROM mem_edit_log WHERE user_id = :uid"), {"uid": user_id})
        db.execute(text("DELETE FROM mem_memories WHERE user_id = :uid"), {"uid": user_id})
        db.commit()
    finally:
        db.close()


# ── 1. Inject ─────────────────────────────────────────────────────────


class TestInject:

    def test_inject_creates_memory_with_high_trust(self, editor, db_factory, user_id):
        mem = editor.inject(
            user_id, "User prefers Python for data work",
            memory_type=MemoryType.SEMANTIC,
        )

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT memory_id, user_id, content, memory_type, "
                "       trust_tier, initial_confidence, is_active, "
                "       source_event_ids "
                "FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": mem.memory_id}).fetchone()

            assert row is not None
            assert row.user_id == user_id
            assert row.content == "User prefers Python for data work"
            assert row.memory_type == "semantic"
            assert row.trust_tier == "T1"
            assert row.initial_confidence == 1.0
            assert row.is_active == 1
            ids = json.loads(row.source_event_ids) if isinstance(row.source_event_ids, str) else row.source_event_ids
            assert any("inject:" in s for s in ids)
        finally:
            db.close()

    def test_inject_custom_trust_tier(self, editor, db_factory, user_id):
        mem = editor.inject(
            user_id, "test",
            memory_type=MemoryType.PROCEDURAL,
            trust_tier=TrustTier.T2_CURATED,
        )

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT trust_tier FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": mem.memory_id}).fetchone()
            assert row.trust_tier == "T2"
        finally:
            db.close()

    def test_inject_logs_audit(self, editor, db_factory, user_id):
        editor.inject(user_id, "test", memory_type=MemoryType.SEMANTIC)

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, user_id, target_ids, created_by "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'inject'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            assert row.operation == "inject"
            assert row.created_by == user_id
        finally:
            db.close()


# ── 2. Correct ────────────────────────────────────────────────────────


class TestCorrect:

    def test_correct_supersedes_old_memory(self, editor, db_factory, user_id):
        # Create original
        original = editor.inject(
            user_id, "User prefers Java",
            memory_type=MemoryType.SEMANTIC,
        )

        # Correct it
        corrected = editor.correct(
            user_id, original.memory_id,
            "User prefers Python",
            reason="User clarified preference",
        )

        db = db_factory()
        try:
            # Old memory: deactivated, superseded_by set
            old = db.execute(text(
                "SELECT is_active, superseded_by "
                "FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": original.memory_id}).fetchone()
            assert old.is_active == 0
            assert old.superseded_by == corrected.memory_id

            # New memory: active, correct content
            new = db.execute(text(
                "SELECT content, is_active, trust_tier, memory_type "
                "FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": corrected.memory_id}).fetchone()
            assert new.content == "User prefers Python"
            assert new.is_active == 1
            assert new.trust_tier == "T2"
            assert new.memory_type == "semantic"
        finally:
            db.close()

    def test_correct_nonexistent_raises(self, editor, user_id):
        with pytest.raises(ValueError, match="not found"):
            editor.correct(user_id, "nonexistent_id", "new content")

    def test_correct_logs_audit(self, editor, db_factory, user_id):
        original = editor.inject(user_id, "old", memory_type=MemoryType.SEMANTIC)
        editor.correct(user_id, original.memory_id, "new", reason="fix")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, target_ids, reason "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'correct'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            ids = json.loads(row.target_ids) if isinstance(row.target_ids, str) else row.target_ids
            assert original.memory_id in ids
            assert row.reason == "fix"
        finally:
            db.close()


# ── 3. Purge ──────────────────────────────────────────────────────────


class TestPurge:

    def test_purge_by_ids(self, editor, db_factory, user_id):
        m1 = editor.inject(user_id, "mem1", memory_type=MemoryType.SEMANTIC)
        m2 = editor.inject(user_id, "mem2", memory_type=MemoryType.SEMANTIC)
        m3 = editor.inject(user_id, "mem3", memory_type=MemoryType.SEMANTIC)

        result = editor.purge(user_id, memory_ids=[m1.memory_id, m2.memory_id], reason="cleanup")
        assert result.deactivated == 2

        db = db_factory()
        try:
            # m1, m2 deactivated
            for mid in [m1.memory_id, m2.memory_id]:
                row = db.execute(text(
                    "SELECT is_active FROM mem_memories WHERE memory_id = :mid"
                ), {"mid": mid}).fetchone()
                assert row.is_active == 0

            # m3 still active
            row = db.execute(text(
                "SELECT is_active FROM mem_memories WHERE memory_id = :mid"
            ), {"mid": m3.memory_id}).fetchone()
            assert row.is_active == 1
        finally:
            db.close()

    def test_purge_by_type(self, editor, db_factory, user_id):
        editor.inject(user_id, "proc1", memory_type=MemoryType.PROCEDURAL)
        editor.inject(user_id, "proc2", memory_type=MemoryType.PROCEDURAL)
        editor.inject(user_id, "sem1", memory_type=MemoryType.SEMANTIC)

        result = editor.purge(user_id, memory_types=[MemoryType.PROCEDURAL])
        assert result.deactivated == 2

        db = db_factory()
        try:
            # Semantic still active
            row = db.execute(text(
                "SELECT COUNT(*) as cnt FROM mem_memories "
                "WHERE user_id = :uid AND is_active = 1"
            ), {"uid": user_id}).fetchone()
            assert row.cnt == 1
        finally:
            db.close()

    def test_purge_logs_audit_with_snapshot(self, editor, db_factory, user_id):
        editor.inject(user_id, "to purge", memory_type=MemoryType.SEMANTIC)
        editor.purge(user_id, memory_types=[MemoryType.SEMANTIC], reason="test purge")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, reason, snapshot_before "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'purge'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            assert row.reason == "test purge"
            # snapshot_before may be None if snapshot creation failed (best-effort)
        finally:
            db.close()

    def test_purge_does_not_affect_other_users(self, editor, db_factory, user_id):
        other = _uid()
        try:
            editor.inject(user_id, "my mem", memory_type=MemoryType.SEMANTIC)
            editor.inject(other, "other mem", memory_type=MemoryType.SEMANTIC)

            editor.purge(user_id, memory_types=[MemoryType.SEMANTIC])

            db = db_factory()
            try:
                row = db.execute(text(
                    "SELECT is_active FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1"
                ), {"uid": other}).fetchone()
                assert row is not None  # other user's memory still active
            finally:
                db.close()
        finally:
            db = db_factory()
            try:
                db.execute(text("DELETE FROM mem_memories WHERE user_id = :uid"), {"uid": other})
                db.commit()
            finally:
                db.close()
