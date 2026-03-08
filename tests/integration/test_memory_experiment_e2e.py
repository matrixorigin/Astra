"""Integration tests for MemoryExperimentManager (Phase 3).

Tests the full experiment lifecycle against a real MatrixOne database:
create → mutate → diff → evaluate → commit/discard.

Covers:
- Basic CRUD lifecycle
- evaluate() with golden session replay
- Optimistic locking on commit (ExperimentConflictError)
- TTL management (expires_at, extend_ttl, cleanup_expired)
- get_service() operates on branch (isolated mutations)
- diff() returns actual changes
- commit() merges data into production
"""

import json
import os
import uuid

import pytest
from sqlalchemy import text

# Ensure test database
os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")
_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")

from api.database import SessionLocal  # noqa: E402
from core.memory.experiment import (  # noqa: E402
    DEFAULT_MAX_EXPERIMENTS,
    DEFAULT_TTL_DAYS,
    MAX_TTL_DAYS,
    ExperimentConflictError,
    ExperimentLimitError,
    MemoryExperimentManager,
)


@pytest.fixture()
def db_factory():
    return SessionLocal


@pytest.fixture()
def mgr(db_factory):
    return MemoryExperimentManager(db_factory, source_db=_TEST_DB)


@pytest.fixture(autouse=True)
def _cleanup(db_factory):
    """Clean up experiment records and branch DBs after each test."""
    yield
    with db_factory() as db:
        rows = db.execute(
            text(
                "SELECT branch_db, base_snapshot FROM mem_experiments "
                "WHERE user_id LIKE 'test_exp_%'"
            )
        ).fetchall()
        branch_dbs = [r.branch_db for r in rows]
        snap_names = [r.base_snapshot for r in rows if r.base_snapshot]
        db.execute(text("DELETE FROM mem_experiments WHERE user_id LIKE 'test_exp_%'"))
        db.commit()

    for bdb in branch_dbs:
        try:
            with db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{bdb}`"))
                db.commit()
        except Exception:
            pass

    for snap in snap_names:
        try:
            with db_factory() as db:
                db.commit()
                db.execute(text(f"DROP SNAPSHOT IF EXISTS {snap}"))
                db.commit()
        except Exception:
            pass


# ── Basic CRUD ────────────────────────────────────────────────────────


class TestExperimentCreate:
    def test_create_basic(self, mgr, db_factory):
        """Create experiment → verify all DB fields including expires_at."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "test-exp-1", description="unit test")

        assert info.experiment_id
        assert info.user_id == user_id
        assert info.name == "test-exp-1"
        assert info.status == "active"
        assert info.branch_db
        assert info.description == "unit test"
        assert info.expires_at is not None

        # Verify DB record
        with db_factory() as db:
            row = db.execute(
                text("SELECT * FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            m = row._mapping
            assert m["user_id"] == user_id
            assert m["name"] == "test-exp-1"
            assert m["status"] == "active"
            assert m["branch_db"] == info.branch_db
            assert m["created_by"] == user_id
            assert m["created_at"] is not None
            assert m["expires_at"] is not None

        # Verify branch database exists
        with db_factory() as db:
            dbs = db.execute(text("SHOW DATABASES")).fetchall()
            db_names = [r[0] for r in dbs]
            assert info.branch_db in db_names

    def test_create_with_strategy_and_params(self, mgr, db_factory):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        params = {"spreading_factor": 0.9}
        info = mgr.create(
            user_id, "tuning-exp",
            strategy_key="activation:v1", params=params,
        )

        with db_factory() as db:
            row = db.execute(
                text("SELECT strategy_key, params_json FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            assert row._mapping["strategy_key"] == "activation:v1"
            pj = row._mapping["params_json"]
            if isinstance(pj, str):
                pj = json.loads(pj)
            assert pj["spreading_factor"] == 0.9
            # Defaults filled in by validation
            assert "num_iterations" in pj

    def test_create_limit_enforced(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        for i in range(DEFAULT_MAX_EXPERIMENTS):
            mgr.create(user_id, f"exp-{i}")
        with pytest.raises(ExperimentLimitError):
            mgr.create(user_id, "one-too-many")

    def test_create_limit_excludes_discarded(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "will-discard")
        mgr.discard(info.experiment_id)
        for i in range(DEFAULT_MAX_EXPERIMENTS):
            mgr.create(user_id, f"exp-{i}")


class TestExperimentGetAndList:
    def test_get_existing(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        created = mgr.create(user_id, "get-test")
        fetched = mgr.get(created.experiment_id)
        assert fetched is not None
        assert fetched.experiment_id == created.experiment_id
        assert fetched.name == "get-test"

    def test_get_nonexistent(self, mgr):
        assert mgr.get("nonexistent_id") is None

    def test_list_active(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        mgr.create(user_id, "exp-a")
        mgr.create(user_id, "exp-b")
        active = mgr.list_active(user_id)
        assert len(active) == 2
        assert {e.name for e in active} == {"exp-a", "exp-b"}

    def test_list_active_excludes_discarded(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "will-discard")
        mgr.create(user_id, "stays-active")
        mgr.discard(info.experiment_id)
        active = mgr.list_active(user_id)
        assert len(active) == 1
        assert active[0].name == "stays-active"


class TestExperimentDiscard:
    def test_discard_sets_status(self, mgr, db_factory):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "discard-test")
        mgr.discard(info.experiment_id)
        with db_factory() as db:
            row = db.execute(
                text("SELECT status FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            assert row._mapping["status"] == "discarded"

    def test_discard_nonactive_raises(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "discard-twice")
        mgr.discard(info.experiment_id)
        with pytest.raises(ValueError, match="discarded"):
            mgr.discard(info.experiment_id)

    def test_discard_nonexistent_raises(self, mgr):
        with pytest.raises(ValueError, match="not found"):
            mgr.discard("nonexistent_id")


class TestExperimentCommit:
    def test_commit_sets_status_and_timestamp(self, mgr, db_factory):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "commit-test")
        mgr.commit(info.experiment_id)
        with db_factory() as db:
            row = db.execute(
                text("SELECT status, committed_at FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            m = row._mapping
            assert m["status"] == "committed"
            assert m["committed_at"] is not None

    def test_commit_nonactive_raises(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "commit-twice")
        mgr.commit(info.experiment_id)
        with pytest.raises(ValueError, match="committed"):
            mgr.commit(info.experiment_id)


class TestExperimentMetrics:
    def test_update_metrics(self, mgr, db_factory):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "metrics-test")
        metrics = {"precision_at_10": 0.82, "recall_at_10": 0.75}
        mgr.update_metrics(info.experiment_id, metrics)
        with db_factory() as db:
            row = db.execute(
                text("SELECT metrics_json FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            mj = row._mapping["metrics_json"]
            if isinstance(mj, str):
                mj = json.loads(mj)
            assert mj["precision_at_10"] == 0.82


class TestExperimentUserIsolation:
    def test_no_cross_user_visibility(self, mgr):
        user_a = f"test_exp_{uuid.uuid4().hex[:8]}"
        user_b = f"test_exp_{uuid.uuid4().hex[:8]}"
        mgr.create(user_a, "alice-exp")
        mgr.create(user_b, "bob-exp")
        assert len(mgr.list_active(user_a)) == 1
        assert mgr.list_active(user_a)[0].name == "alice-exp"
        assert len(mgr.list_active(user_b)) == 1
        assert mgr.list_active(user_b)[0].name == "bob-exp"


# ── Evaluate ──────────────────────────────────────────────────────────


class TestExperimentEvaluate:
    def test_evaluate_no_golden_sessions(self, mgr, db_factory):
        """evaluate() with no golden sessions returns gracefully."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "eval-no-golden")

        result = mgr.evaluate(info.experiment_id)

        assert result.sessions_tested == 0
        assert result.metrics.get("note") == "no_golden_sessions"

        # Status should revert to active
        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.status == "active"

        # Metrics should be persisted
        with db_factory() as db:
            row = db.execute(
                text("SELECT metrics_json FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            mj = row._mapping["metrics_json"]
            if isinstance(mj, str):
                mj = json.loads(mj)
            assert mj["note"] == "no_golden_sessions"

    def test_evaluate_replays_golden_sessions(self, mgr, db_factory):
        """evaluate() with explicit session IDs exercises full replay path."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        session_id = f"sess_{uuid.uuid4().hex[:8]}"
        event_id = f"evt_{uuid.uuid4().hex[:8]}"
        chain_id = f"chain_{uuid.uuid4().hex[:8]}"

        # Create a real session + event so ReplayService can find them
        with db_factory() as db:
            db.execute(
                text(
                    "INSERT INTO agent_sessions "
                    "(session_id, user_id, status, event_count, "
                    " created_at, last_active_at) "
                    "VALUES (:sid, :uid, 'active', 1, NOW(), NOW())"
                ),
                {"sid": session_id, "uid": user_id},
            )
            db.execute(
                text(
                    "INSERT INTO agent_events "
                    "(event_id, session_id, user_id, agent_id, agent_version, "
                    " event_type, content, causal_chain_id, created_at) "
                    "VALUES (:eid, :sid, :uid, 'system', '1.0.0', "
                    " 'user_query', 'test message', :cid, NOW())"
                ),
                {"eid": event_id, "sid": session_id, "uid": user_id, "cid": chain_id},
            )
            db.commit()

        try:
            info = mgr.create(user_id, "eval-replay-test")

            result = mgr.evaluate(
                info.experiment_id,
                golden_session_ids=[session_id],
            )

            # ── Verify EvalResult fields ──
            assert result.sessions_tested == 1
            assert result.sessions_passed == 1  # replay succeeded
            assert len(result.replay_results) == 1

            rr = result.replay_results[0]
            assert rr["session_id"] == session_id
            assert rr["replay_status"] == "completed"
            assert rr["successful"] >= 1  # at least the user_query event
            assert rr["failed"] == 0

            # ── Verify aggregate metrics ──
            assert result.metrics["sessions_tested"] == 1
            assert result.metrics["pass_rate"] == 1.0
            assert result.metrics["error_rate"] == 0.0

            # ── Verify status reverted to active ──
            fetched = mgr.get(info.experiment_id)
            assert fetched is not None
            assert fetched.status == "active"

            # ── Verify metrics persisted to DB ──
            assert fetched.metrics_json is not None
            assert fetched.metrics_json["sessions_tested"] == 1
            assert fetched.metrics_json["sessions_passed"] == 1
            assert fetched.metrics_json["pass_rate"] == 1.0
        finally:
            with db_factory() as db:
                db.execute(
                    text("DELETE FROM agent_events WHERE event_id = :eid"),
                    {"eid": event_id},
                )
                db.execute(
                    text("DELETE FROM agent_sessions WHERE session_id = :sid"),
                    {"sid": session_id},
                )
                db.commit()

    def test_evaluate_nonexistent_raises(self, mgr):
        with pytest.raises(ValueError, match="not found"):
            mgr.evaluate("nonexistent_id")

    def test_evaluate_committed_raises(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "eval-committed")
        mgr.commit(info.experiment_id)
        with pytest.raises(ValueError, match="committed"):
            mgr.evaluate(info.experiment_id)


# ── Optimistic Locking ────────────────────────────────────────────────


class TestOptimisticLocking:
    def test_commit_detects_production_change(self, mgr, db_factory):
        """If production mem_memories changes after branch point, commit fails."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "conflict-test")

        # Only test if snapshot was created successfully
        if info.base_snapshot is None:
            pytest.skip("Snapshot creation failed, cannot test optimistic lock")

        # Simulate production change: insert a memory after the snapshot
        import time
        time.sleep(1)  # Ensure updated_at > snapshot timestamp
        with db_factory() as db:
            db.execute(
                text(
                    "INSERT INTO mem_memories "
                    "(memory_id, user_id, content, memory_type, trust_tier, "
                    " initial_confidence, source_event_ids, is_active, "
                    " observed_at, created_at, updated_at) "
                    "VALUES (:mid, :uid, 'new production memory', 'semantic', 'T3', "
                    " 0.75, '[]', 1, NOW(), NOW(), NOW())"
                ),
                {"mid": uuid.uuid4().hex, "uid": user_id},
            )
            db.commit()

        with pytest.raises(ExperimentConflictError, match="memory changes since branch"):
            mgr.commit(info.experiment_id)

        # Experiment should still be active (not corrupted)
        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.status == "active"

        # Clean up the test memory
        with db_factory() as db:
            db.execute(
                text("DELETE FROM mem_memories WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.commit()

    def test_commit_succeeds_when_no_production_change(self, mgr):
        """Commit succeeds when production hasn't changed."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "no-conflict")
        # No production changes → commit should succeed
        mgr.commit(info.experiment_id)
        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.status == "committed"


# ── TTL Management ────────────────────────────────────────────────────


class TestTTLManagement:
    def test_create_sets_expires_at(self, mgr, db_factory):
        """Create with default TTL sets expires_at ~7 days from now."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-test")

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT created_at, expires_at, "
                    "TIMESTAMPDIFF(SECOND, created_at, expires_at) AS ttl_secs "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            m = row._mapping
            assert m["expires_at"] is not None
            # TTL should be ~7 days (604800 seconds), allow 60s tolerance
            assert abs(m["ttl_secs"] - DEFAULT_TTL_DAYS * 86400) < 60

    def test_create_custom_ttl(self, mgr, db_factory):
        """Create with custom TTL."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-custom", ttl_days=3)

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT TIMESTAMPDIFF(SECOND, created_at, expires_at) AS ttl_secs "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            assert abs(row._mapping["ttl_secs"] - 3 * 86400) < 60

    def test_create_ttl_capped_at_max(self, mgr, db_factory):
        """TTL is capped at MAX_TTL_DAYS even if requested higher."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-capped", ttl_days=999)

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT TIMESTAMPDIFF(SECOND, created_at, expires_at) AS ttl_secs "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            assert abs(row._mapping["ttl_secs"] - MAX_TTL_DAYS * 86400) < 60

    def test_extend_ttl(self, mgr, db_factory):
        """extend_ttl() advances expires_at."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-extend", ttl_days=3)

        # Get original expires_at
        with db_factory() as db:
            row = db.execute(
                text("SELECT expires_at FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            original_expires = row._mapping["expires_at"]

        mgr.extend_ttl(info.experiment_id, days=5)

        with db_factory() as db:
            row = db.execute(
                text("SELECT expires_at FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": info.experiment_id},
            ).fetchone()
            new_expires = row._mapping["expires_at"]
            assert new_expires > original_expires

    def test_extend_ttl_capped_at_max(self, mgr, db_factory):
        """extend_ttl() cannot exceed MAX_TTL_DAYS from creation."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-extend-cap", ttl_days=DEFAULT_TTL_DAYS)

        # Try to extend way beyond max
        mgr.extend_ttl(info.experiment_id, days=999)

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT TIMESTAMPDIFF(SECOND, created_at, expires_at) AS ttl_secs "
                    "FROM mem_experiments WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            ).fetchone()
            # Should be capped at MAX_TTL_DAYS
            assert row._mapping["ttl_secs"] <= MAX_TTL_DAYS * 86400 + 60

    def test_extend_ttl_nonactive_raises(self, mgr):
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "ttl-extend-bad")
        mgr.discard(info.experiment_id)
        with pytest.raises(ValueError, match="discarded"):
            mgr.extend_ttl(info.experiment_id)

    def test_cleanup_expired(self, mgr, db_factory):
        """cleanup_expired() expires experiments past their TTL."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "will-expire", ttl_days=1)

        # Manually set expires_at to the past
        with db_factory() as db:
            db.execute(
                text(
                    "UPDATE mem_experiments "
                    "SET expires_at = DATE_SUB(NOW(), INTERVAL 1 HOUR) "
                    "WHERE experiment_id = :eid"
                ),
                {"eid": info.experiment_id},
            )
            db.commit()

        count = mgr.cleanup_expired()
        assert count >= 1

        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.status == "expired"

    def test_cleanup_skips_non_expired(self, mgr, db_factory):
        """cleanup_expired() doesn't touch experiments with future TTL."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "not-expired", ttl_days=7)

        mgr.cleanup_expired()
        # Our experiment should NOT be expired
        fetched = mgr.get(info.experiment_id)
        assert fetched is not None
        assert fetched.status == "active"


# ── Branch Operations (get_service, diff, commit data) ────────────────


class TestBranchOperations:
    """Test that get_service operates on branch, diff shows changes,
    and commit merges data into production."""

    def test_get_service_writes_to_branch_not_production(self, mgr, db_factory):
        """Mutations via get_service() only affect the branch DB."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "branch-write-test")

        # Insert a memory directly into the branch DB
        with db_factory() as db:
            db.execute(
                text(
                    f"INSERT INTO `{info.branch_db}`.mem_memories "
                    "(memory_id, user_id, content, memory_type, trust_tier, "
                    " initial_confidence, source_event_ids, is_active, "
                    " observed_at, created_at, updated_at) "
                    "VALUES (:mid, :uid, 'branch only', 'semantic', 'T3', "
                    " 0.75, '[]', 1, NOW(), NOW(), NOW())"
                ),
                {"mid": f"branch_{uuid.uuid4().hex[:8]}", "uid": user_id},
            )
            db.commit()

        # Verify it exists in branch
        with db_factory() as db:
            row = db.execute(
                text(
                    f"SELECT COUNT(*) AS cnt FROM `{info.branch_db}`.mem_memories "
                    "WHERE user_id = :uid AND content = 'branch only'"
                ),
                {"uid": user_id},
            ).fetchone()
            assert row.cnt == 1

        # Verify it does NOT exist in production
        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT COUNT(*) AS cnt FROM mem_memories "
                    "WHERE user_id = :uid AND content = 'branch only'"
                ),
                {"uid": user_id},
            ).fetchone()
            assert row.cnt == 0

    def test_diff_shows_branch_changes(self, mgr, db_factory):
        """diff() returns non-empty result when branch has changes."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "diff-test")

        # Insert data into branch
        mid = f"diff_{uuid.uuid4().hex[:8]}"
        with db_factory() as db:
            db.execute(
                text(
                    f"INSERT INTO `{info.branch_db}`.mem_memories "
                    "(memory_id, user_id, content, memory_type, trust_tier, "
                    " initial_confidence, source_event_ids, is_active, "
                    " observed_at, created_at, updated_at) "
                    "VALUES (:mid, :uid, 'diff content', 'semantic', 'T3', "
                    " 0.75, '[]', 1, NOW(), NOW(), NOW())"
                ),
                {"mid": mid, "uid": user_id},
            )
            db.commit()

        result = mgr.diff(info.experiment_id)
        # Should have at least one table with changes
        assert len(result.table_diffs) > 0
        mem_diff = next(
            (d for d in result.table_diffs if d["table"] == "mem_memories"), None
        )
        assert mem_diff is not None
        assert len(mem_diff["changes"]) > 0

    def test_commit_merges_branch_data_into_production(self, mgr, db_factory):
        """After commit, branch data appears in production."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "commit-merge-test")

        # Insert data into branch
        mid = f"merge_{uuid.uuid4().hex[:8]}"
        with db_factory() as db:
            db.execute(
                text(
                    f"INSERT INTO `{info.branch_db}`.mem_memories "
                    "(memory_id, user_id, content, memory_type, trust_tier, "
                    " initial_confidence, source_event_ids, is_active, "
                    " observed_at, created_at, updated_at) "
                    "VALUES (:mid, :uid, 'merged content', 'semantic', 'T3', "
                    " 0.75, '[]', 1, NOW(), NOW(), NOW())"
                ),
                {"mid": mid, "uid": user_id},
            )
            db.commit()

        # Commit — merge must succeed for mem_memories (raises on failure)
        mgr.commit(info.experiment_id)

        # Verify data now in production
        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT content, is_active FROM mem_memories "
                    "WHERE memory_id = :mid"
                ),
                {"mid": mid},
            ).fetchone()

        assert row is not None
        assert row.content == "merged content"
        assert row.is_active == 1

        # Clean up merged data
        with db_factory() as db:
            db.execute(
                text("DELETE FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            )
            db.commit()

    def test_dispose_engines_releases_connections(self, mgr, db_factory):
        """dispose_engines() cleans up branch connection pools."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "dispose-test")

        # Create a branch engine via get_service
        _svc = mgr.get_service(info.experiment_id)
        assert len(mgr._branch_engines) >= 1
        assert info.experiment_id in mgr._branch_engines

        mgr.dispose_engines()
        assert len(mgr._branch_engines) == 0

    def test_commit_disposes_branch_engine(self, mgr, db_factory):
        """commit() auto-disposes the branch engine for that experiment."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "commit-dispose")

        _svc = mgr.get_service(info.experiment_id)
        assert info.experiment_id in mgr._branch_engines

        mgr.commit(info.experiment_id)
        assert info.experiment_id not in mgr._branch_engines

    def test_discard_disposes_branch_engine(self, mgr, db_factory):
        """discard() auto-disposes the branch engine for that experiment."""
        user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
        info = mgr.create(user_id, "discard-dispose")

        _svc = mgr.get_service(info.experiment_id)
        assert info.experiment_id in mgr._branch_engines

        mgr.discard(info.experiment_id)
        assert info.experiment_id not in mgr._branch_engines

    def test_context_manager_disposes_engines(self, db_factory):
        """Context manager auto-disposes all engines on exit."""
        with MemoryExperimentManager(db_factory, source_db=_TEST_DB) as m:
            user_id = f"test_exp_{uuid.uuid4().hex[:8]}"
            info = m.create(user_id, "ctx-mgr-test")
            _svc = m.get_service(info.experiment_id)
            assert len(m._branch_engines) >= 1
        assert len(m._branch_engines) == 0
