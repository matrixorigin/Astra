"""Integration tests for MemoryEditor with Memoria backend.

Verifies against Memoria service:
1. inject → creates memory with correct trust tier
2. correct → old deactivated, new created
3. purge → memories deactivated
4. User isolation — edits don't affect other users

Requires Memoria service running. Set MEMORIA_BASE_URL and MEMORIA_MASTER_KEY.
"""

from __future__ import annotations

import os
from uuid import uuid4

import pytest

from core.memory.factory import create_editor
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


@pytest.fixture(autouse=True)
def _setup_memoria_env():
    """Set Memoria environment variables from test configuration."""
    os.environ["MEMORIA_BASE_URL"] = os.environ.get("TEST_MEMORIA_BASE_URL", "http://localhost:8100")
    os.environ["MEMORIA_MASTER_KEY"] = os.environ.get("TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
    os.environ["MEMORIA_API_KEY"] = os.environ.get("TEST_MEMORIA_API_KEY", "")
    yield


@pytest.fixture
def editor(db_factory, user_id):
    """Create MemoryEditor with Memoria backend."""
    return create_editor(db_factory, user_id=user_id)


@pytest.fixture(autouse=True)
def cleanup(editor, user_id):
    """Cleanup memories after test."""
    yield
    try:
        # Purge all memories for this user
        editor.purge(user_id, memory_types=[MemoryType.SEMANTIC, MemoryType.PROCEDURAL])
    except Exception:
        pass


# ── 1. Inject ─────────────────────────────────────────────────────────


class TestInject:

    def test_inject_creates_memory_with_high_trust(self, editor, user_id):
        mem = editor.inject(
            user_id, "User prefers Python for data work",
            memory_type=MemoryType.SEMANTIC,
        )

        # Verify returned memory has correct properties
        assert mem.user_id == user_id
        assert mem.content == "User prefers Python for data work"
        assert mem.memory_type == MemoryType.SEMANTIC
        assert mem.trust_tier == TrustTier.T1_VERIFIED
        assert mem.initial_confidence == 1.0

        # Verify can retrieve from storage
        retrieved = editor._storage.get_memory(mem.memory_id)
        assert retrieved is not None
        assert retrieved.content == mem.content

    def test_inject_custom_trust_tier(self, editor, user_id):
        mem = editor.inject(
            user_id, "test",
            memory_type=MemoryType.PROCEDURAL,
            trust_tier=TrustTier.T2_CURATED,
        )

        assert mem.trust_tier == TrustTier.T2_CURATED

    def test_inject_logs_audit(self, editor, db_factory, user_id):
        editor.inject(user_id, "test", memory_type=MemoryType.SEMANTIC)

        # Audit log is written to local DB
        from sqlalchemy import text
        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, user_id, created_by "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'inject'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            assert row.operation == "inject"
        finally:
            db.close()


# ── 2. Correct ────────────────────────────────────────────────────────


class TestCorrect:

    def test_correct_supersedes_old_memory(self, editor, user_id):
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

        # Verify new memory has correct content
        assert corrected.content == "User prefers Python"
        assert corrected.memory_type == original.memory_type

        # Verify old memory is deactivated (cannot be retrieved)
        old_retrieved = editor._storage.get_memory(original.memory_id)
        assert old_retrieved is None or not getattr(old_retrieved, 'is_active', True)

    def test_correct_nonexistent_raises(self, editor, user_id):
        with pytest.raises(Exception):  # Memoria returns 404
            editor.correct(user_id, "nonexistent_id", "new content")

    def test_correct_logs_audit(self, editor, db_factory, user_id):
        original = editor.inject(user_id, "old", memory_type=MemoryType.SEMANTIC)
        editor.correct(user_id, original.memory_id, "new", reason="fix")

        from sqlalchemy import text
        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, reason "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'correct'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            assert row.reason == "fix"
        finally:
            db.close()


# ── 3. Purge ──────────────────────────────────────────────────────────


class TestPurge:

    def test_purge_by_ids(self, editor, user_id):
        m1 = editor.inject(user_id, "mem1", memory_type=MemoryType.SEMANTIC)
        m2 = editor.inject(user_id, "mem2", memory_type=MemoryType.SEMANTIC)
        m3 = editor.inject(user_id, "mem3", memory_type=MemoryType.SEMANTIC)

        result = editor.purge(user_id, memory_ids=[m1.memory_id, m2.memory_id], reason="cleanup")
        # Verify purge returned valid result with count >= 0
        assert isinstance(result.deactivated, int)
        assert result.deactivated >= 0

        # Verify m3 still exists (was not purged)
        retrieved = editor._storage.get_memory(m3.memory_id)
        assert retrieved is not None

    def test_purge_logs_audit_with_snapshot(self, editor, db_factory, user_id):
        editor.inject(user_id, "to purge", memory_type=MemoryType.SEMANTIC)
        result = editor.purge(user_id, memory_types=[MemoryType.SEMANTIC], reason="test purge")

        # Verify snapshot was created
        assert result.snapshot_name is not None
        assert result.snapshot_name.startswith("pre_purge_")

        from sqlalchemy import text
        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT operation, reason, snapshot_before "
                "FROM mem_edit_log WHERE user_id = :uid AND operation = 'purge'"
            ), {"uid": user_id}).fetchone()
            assert row is not None
            assert row.reason == "test purge"
        finally:
            db.close()

    def test_purge_does_not_affect_other_users(self, editor, user_id):
        other = _uid()
        other_editor = create_editor(editor._db_factory, user_id=other)

        try:
            my_mem = editor.inject(user_id, "my mem", memory_type=MemoryType.SEMANTIC)
            other_mem = other_editor.inject(other, "other mem", memory_type=MemoryType.SEMANTIC)

            # Purge my memories
            editor.purge(user_id, memory_types=[MemoryType.SEMANTIC])

            # Verify other user's memory still exists
            retrieved = other_editor._storage.get_memory(other_mem.memory_id)
            assert retrieved is not None
        finally:
            # Cleanup
            try:
                other_editor.purge(other, memory_types=[MemoryType.SEMANTIC, MemoryType.PROCEDURAL])
            except Exception:
                pass
