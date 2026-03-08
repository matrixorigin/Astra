"""Integration tests for per-user strategy binding (Phase 1).

Verifies against REAL DB:
1. mem_user_memory_config table creation and CRUD
2. set_user_strategy persists correctly
3. create_memory_service resolves per-user strategy from DB
4. switch_user_strategy with backfill flow
5. Field-level verification of all columns
"""

from uuid import uuid4

import pytest
from sqlalchemy import text

from api.models._constants import EMBEDDING_DIM
from core.memory.factory import (
    create_memory_service,
    set_user_strategy,
    switch_user_strategy,
)
from core.memory.types import MemoryType


def _uid() -> str:
    return f"ucfg_e2e_{uuid4().hex[:12]}"


def _embed(seed: float = 0.1) -> list[float]:
    return [seed] * EMBEDDING_DIM


@pytest.fixture
def db_factory():
    from api.database import SessionLocal
    return SessionLocal


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    """Cleanup test data after each test."""
    yield
    db = db_factory()
    try:
        db.execute(text(
            "DELETE FROM mem_user_memory_config WHERE user_id = :uid"
        ), {"uid": user_id})
        db.execute(text(
            "DELETE FROM memory_graph_edges WHERE user_id = :uid"
        ), {"uid": user_id})
        db.execute(text(
            "DELETE FROM memory_graph_nodes WHERE user_id = :uid"
        ), {"uid": user_id})
        db.execute(text(
            "DELETE FROM mem_memories WHERE user_id = :uid"
        ), {"uid": user_id})
        db.commit()
    finally:
        db.close()


# ── 1. Table exists and CRUD works ───────────────────────────────────


class TestMemUserMemoryConfigTable:
    """Verify mem_user_memory_config table schema and basic CRUD."""

    def test_table_exists(self, db_factory):
        db = db_factory()
        try:
            result = db.execute(text(
                "SELECT COUNT(*) FROM mem_user_memory_config WHERE 1=0"
            )).fetchone()
            assert result is not None
        finally:
            db.close()

    def test_set_user_strategy_creates_row(self, db_factory, user_id):
        set_user_strategy(db_factory, user_id, "activation:v1")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT user_id, strategy_key, params_json, index_status, "
                "       migrated_from, migration_snapshot, "
                "       created_at, updated_at "
                "FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()

            assert row is not None
            assert row.user_id == user_id
            assert row.strategy_key == "activation:v1"
            assert row.params_json is None
            assert row.index_status == "ready"
            assert row.migrated_from is None
            assert row.migration_snapshot is None
            assert row.created_at is not None
            assert row.updated_at is not None
        finally:
            db.close()

    def test_set_user_strategy_updates_existing(self, db_factory, user_id):
        set_user_strategy(db_factory, user_id, "vector:v1")

        db = db_factory()
        try:
            row_before = db.execute(text(
                "SELECT updated_at FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
        finally:
            db.close()

        import time
        time.sleep(0.01)  # ensure timestamp differs
        set_user_strategy(db_factory, user_id, "activation:v1")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT strategy_key, updated_at FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert row.strategy_key == "activation:v1"
            # updated_at must have advanced
            assert row.updated_at >= row_before.updated_at

            # Only one row (upsert, not duplicate)
            count = db.execute(text(
                "SELECT COUNT(*) as cnt FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert count.cnt == 1
        finally:
            db.close()


# ── 2. Factory resolves per-user strategy from DB ─────────────────────


class TestFactoryPerUserResolution:
    """create_memory_service uses DB row when user_id is provided."""

    def test_resolves_from_db(self, db_factory, user_id):
        set_user_strategy(db_factory, user_id, "activation:v1")

        svc = create_memory_service(db_factory, user_id=user_id)
        assert svc.strategy_key == "activation:v1"

    def test_no_db_row_uses_default(self, db_factory, user_id):
        svc = create_memory_service(db_factory, user_id=user_id)
        assert svc.strategy_key == "vector:v1"

    def test_explicit_strategy_overrides_db(self, db_factory, user_id):
        set_user_strategy(db_factory, user_id, "activation:v1")

        svc = create_memory_service(
            db_factory, user_id=user_id, strategy="vector:v1",
        )
        assert svc.strategy_key == "vector:v1"

    def test_different_users_different_strategies(self, db_factory):
        alice = _uid()
        bob = _uid()
        try:
            set_user_strategy(db_factory, alice, "activation:v1")
            # bob has no row → default

            svc_alice = create_memory_service(db_factory, user_id=alice)
            svc_bob = create_memory_service(db_factory, user_id=bob)

            assert svc_alice.strategy_key == "activation:v1"
            assert svc_bob.strategy_key == "vector:v1"
        finally:
            db = db_factory()
            try:
                db.execute(text(
                    "DELETE FROM mem_user_memory_config WHERE user_id IN (:a, :b)"
                ), {"a": alice, "b": bob})
                db.commit()
            finally:
                db.close()


# ── 3. Strategy switch with backfill ──────────────────────────────────


class TestSwitchUserStrategyE2E:
    """switch_user_strategy end-to-end with real DB."""

    def test_switch_vector_to_activation_with_backfill(self, db_factory, user_id):
        """Store memories, switch to activation, verify backfill creates graph nodes."""
        # Store some memories first
        svc = create_memory_service(db_factory, strategy="vector:v1")
        for i in range(3):
            svc.store(
                user_id, f"test memory {i}",
                memory_type=MemoryType.SEMANTIC,
                session_id=f"sess_{i}",
            )

        # Switch to activation (triggers backfill)
        result = switch_user_strategy(db_factory, user_id, "activation:v1")
        assert result.status == "ready"
        assert result.strategy_key == "activation:v1"

        # Verify DB row
        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT strategy_key, index_status "
                "FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert row.strategy_key == "activation:v1"
            assert row.index_status == "ready"

            # Verify graph nodes were created by backfill
            node_count = db.execute(text(
                "SELECT COUNT(*) as cnt FROM memory_graph_nodes WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert node_count.cnt >= 3
        finally:
            db.close()

    def test_switch_to_vector_no_backfill(self, db_factory, user_id):
        """Switching to vector:v1 needs no backfill (reads mem_memories directly)."""
        set_user_strategy(db_factory, user_id, "activation:v1")

        result = switch_user_strategy(db_factory, user_id, "vector:v1")
        assert result.status == "ready"
        assert result.strategy_key == "vector:v1"
        assert result.previous_key == "activation:v1"

    def test_switch_same_strategy_noop(self, db_factory, user_id):
        set_user_strategy(db_factory, user_id, "vector:v1")

        result = switch_user_strategy(db_factory, user_id, "vector:v1")
        assert result.status == "ready"
        assert result.previous_key == "vector:v1"

    def test_switch_updates_migrated_from(self, db_factory, user_id):
        """After switch, migrated_from records the previous strategy."""
        set_user_strategy(db_factory, user_id, "vector:v1")

        # Store a memory so backfill has something to do
        svc = create_memory_service(db_factory, strategy="vector:v1")
        svc.store(
            user_id, "test memory",
            memory_type=MemoryType.SEMANTIC,
        )

        switch_user_strategy(db_factory, user_id, "activation:v1")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT migrated_from FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert row.migrated_from == "vector:v1"
        finally:
            db.close()


# ── 4. Backfill status transitions (DB ground truth) ─────────────────


class TestBackfillStatusTransitions:
    """Verify index_status transitions are persisted correctly in DB."""

    def test_backfill_transitions_to_ready(self, db_factory, user_id):
        """After successful backfill: index_status='ready', not 'backfilling'."""
        svc = create_memory_service(db_factory, strategy="vector:v1")
        svc.store(user_id, "memory for backfill", memory_type=MemoryType.SEMANTIC)

        switch_user_strategy(db_factory, user_id, "activation:v1")

        db = db_factory()
        try:
            row = db.execute(text(
                "SELECT strategy_key, index_status "
                "FROM mem_user_memory_config WHERE user_id = :uid"
            ), {"uid": user_id}).fetchone()
            assert row.strategy_key == "activation:v1"
            assert row.index_status == "ready"
        finally:
            db.close()

    def test_backfilling_status_causes_factory_fallback(self, db_factory, user_id):
        """If index_status='backfilling', factory falls through to default."""
        # Manually insert a backfilling row
        db = db_factory()
        try:
            db.execute(text(
                "INSERT INTO mem_user_memory_config "
                "(user_id, strategy_key, index_status, migrated_from) "
                "VALUES (:uid, 'activation:v1', 'backfilling', 'vector:v1')"
            ), {"uid": user_id})
            db.commit()
        finally:
            db.close()

        # Factory should NOT use activation:v1 — should fall through
        svc = create_memory_service(db_factory, user_id=user_id)
        assert svc.strategy_key == "vector:v1"

    def test_ready_status_used_by_factory(self, db_factory, user_id):
        """If index_status='ready', factory uses the DB strategy."""
        db = db_factory()
        try:
            db.execute(text(
                "INSERT INTO mem_user_memory_config "
                "(user_id, strategy_key, index_status) "
                "VALUES (:uid, 'activation:v1', 'ready')"
            ), {"uid": user_id})
            db.commit()
        finally:
            db.close()

        svc = create_memory_service(db_factory, user_id=user_id)
        assert svc.strategy_key == "activation:v1"

    def test_invalid_strategy_key_rejected(self, db_factory, user_id):
        """Switching to a non-existent strategy raises ValueError."""
        with pytest.raises(ValueError, match="Unknown strategy"):
            switch_user_strategy(db_factory, user_id, "nonexistent:v99")

    def test_no_side_effects_on_other_users(self, db_factory, user_id):
        """Switching one user's strategy doesn't affect others."""
        other = _uid()
        try:
            set_user_strategy(db_factory, other, "vector:v1")
            set_user_strategy(db_factory, user_id, "activation:v1")

            db = db_factory()
            try:
                row = db.execute(text(
                    "SELECT strategy_key FROM mem_user_memory_config WHERE user_id = :uid"
                ), {"uid": other}).fetchone()
                assert row.strategy_key == "vector:v1"
            finally:
                db.close()
        finally:
            db = db_factory()
            try:
                db.execute(text(
                    "DELETE FROM mem_user_memory_config WHERE user_id = :uid"
                ), {"uid": other})
                db.commit()
            finally:
                db.close()
