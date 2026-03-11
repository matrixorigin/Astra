"""End-to-end tests for MemoryProgramTool (CLI layer).

Verifies the full loop: tool execute → programmer → editor → DB,
with field-level ground truth checks on every affected table.
"""

import asyncio
import json
import os

import pytest
from sqlalchemy import text

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")

from api.database import SessionLocal
from cli.tools.memory_program import MemoryProgramTool
from core.utils.id_generator import generate_id


@pytest.fixture()
def tool():
    return MemoryProgramTool()


@pytest.fixture()
def db_factory():
    return SessionLocal


@pytest.fixture()
def uid():
    return f"test_cli_{generate_id()}"


@pytest.fixture(autouse=True)
def _cleanup(uid, db_factory):
    yield
    with db_factory() as db:
        db.execute(text("DELETE FROM mem_memories WHERE user_id = :uid"), {"uid": uid})
        db.execute(text("DELETE FROM mem_edit_log WHERE user_id = :uid"), {"uid": uid})
        db.execute(text("DELETE FROM mem_experiments WHERE user_id = :uid"), {"uid": uid})
        db.execute(text("DELETE FROM mem_user_memory_config WHERE user_id = :uid"), {"uid": uid})
        db.commit()


def _run(coro):
    return asyncio.run(coro)


def _call(tool, **kwargs):
    return json.loads(_run(tool.execute(**kwargs)))


# ── Inject: full field-level verification ─────────────────────────────


class TestInjectE2E:
    def test_inject_single_all_fields(self, tool, uid, db_factory):
        """Inject one memory → verify every column in mem_memories + mem_edit_log."""
        result = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "Python uses 4-space indent", "type": "semantic", "trust": "T1"}}],
            sandbox=False,
            explain=True,
        )

        assert result["actions_executed"] == 1
        assert result["actions_failed"] == 0
        assert len(result["explain"]) == 1
        exp = result["explain"][0]
        assert exp["action"] == "inject"
        assert exp["success"] is True
        mid = exp["detail"]["memory_id"]
        assert len(mid) == 32  # full uuid hex

        # Ground truth: mem_memories
        with db_factory() as db:
            row = db.execute(
                text("SELECT * FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            ).fetchone()

        assert row is not None
        assert row.memory_id == mid
        assert row.user_id == uid
        assert row.session_id is None
        assert row.memory_type == "semantic"
        assert row.content == "Python uses 4-space indent"
        assert row.initial_confidence == 1.0  # editor.inject sets 1.0
        assert row.trust_tier == "T1"
        # embedding is generated when an embedding client is available
        assert row.source_event_ids is not None  # JSON array
        assert row.superseded_by is None
        assert row.is_active == 1
        assert row.observed_at is not None
        assert row.created_at is not None

        # Ground truth: mem_edit_log
        with db_factory() as db:
            log = db.execute(
                text(
                    "SELECT * FROM mem_edit_log "
                    "WHERE user_id = :uid AND operation = 'inject' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"uid": uid},
            ).fetchone()

        assert log is not None
        assert log.user_id == uid
        assert log.operation == "inject"
        assert mid in str(log.target_ids)  # target_ids JSON contains the memory_id
        assert log.created_at is not None
        assert log.created_by == uid

    def test_inject_with_trust_t2(self, tool, uid, db_factory):
        """Inject with T2 trust → verify confidence from TRUST_TIER_INITIAL_CONFIDENCE."""
        result = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "fact", "type": "procedural", "trust": "T2"}}],
            sandbox=False,
            explain=True,
        )
        mid = result["explain"][0]["detail"]["memory_id"]

        with db_factory() as db:
            row = db.execute(
                text("SELECT memory_type, trust_tier, initial_confidence FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            ).fetchone()

        assert row.memory_type == "procedural"
        assert row.trust_tier == "T2"
        # T2 default confidence from TRUST_TIER_INITIAL_CONFIDENCE
        assert 0.7 <= row.initial_confidence <= 1.0

    def test_inject_no_side_effects(self, tool, uid, db_factory):
        """Inject for uid X → no records created for other users."""
        other_uid = f"test_cli_{generate_id()}"
        _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "only for me"}}],
            sandbox=False,
        )

        with db_factory() as db:
            count = db.execute(
                text("SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid"),
                {"uid": other_uid},
            ).scalar()
        assert count == 0


# ── Correct: old deactivated, new created, linked ─────────────────────


class TestCorrectE2E:
    def test_correct_full_lifecycle(self, tool, uid, db_factory):
        """Inject → correct → verify old deactivated, new created, superseded_by linked."""
        # Step 1: inject
        r1 = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "Earth is flat", "type": "semantic"}}],
            sandbox=False,
            explain=True,
        )
        old_mid = r1["explain"][0]["detail"]["memory_id"]

        # Step 2: correct
        r2 = _call(
            tool,
            user_id=uid,
            actions=[{"correct": {"memory_id": old_mid, "new_content": "Earth is round", "reason": "factual error"}}],
            sandbox=False,
            explain=True,
        )
        assert r2["actions_executed"] == 1
        assert r2["actions_failed"] == 0
        new_mid = r2["explain"][0]["detail"]["new_id"]
        assert r2["explain"][0]["detail"]["old_id"] == old_mid

        # Ground truth: old memory deactivated + superseded_by set
        with db_factory() as db:
            old = db.execute(
                text("SELECT is_active, superseded_by, content FROM mem_memories WHERE memory_id = :mid"),
                {"mid": old_mid},
            ).fetchone()

        assert old.is_active == 0
        assert old.superseded_by == new_mid
        assert old.content == "Earth is flat"  # content unchanged

        # Ground truth: new memory active
        with db_factory() as db:
            new = db.execute(
                text("SELECT * FROM mem_memories WHERE memory_id = :mid"),
                {"mid": new_mid},
            ).fetchone()

        assert new.is_active == 1
        assert new.content == "Earth is round"
        assert new.user_id == uid
        assert new.memory_type == "semantic"  # inherited from old
        assert new.superseded_by is None

        # Ground truth: edit log has correct entry
        with db_factory() as db:
            log = db.execute(
                text(
                    "SELECT * FROM mem_edit_log "
                    "WHERE user_id = :uid AND operation = 'correct' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"uid": uid},
            ).fetchone()

        assert log is not None
        assert old_mid in str(log.target_ids)
        assert new_mid in str(log.target_ids)


# ── Purge: deactivation + snapshot + edit log ─────────────────────────


class TestPurgeE2E:
    def test_purge_by_type(self, tool, uid, db_factory):
        """Inject 2 semantic + 1 procedural → purge semantic → only semantic deactivated."""
        r1 = _call(tool, user_id=uid, actions=[
            {"inject": {"content": "sem1", "type": "semantic"}},
            {"inject": {"content": "sem2", "type": "semantic"}},
            {"inject": {"content": "proc1", "type": "procedural"}},
        ], sandbox=False, explain=True)
        # Batch coalesced → 1 result with memory_ids
        all_ids = r1["explain"][0]["detail"]["memory_ids"]
        assert len(all_ids) == 3

        r = _call(
            tool,
            user_id=uid,
            actions=[{"purge": {"filter": {"type": "semantic"}, "reason": "cleanup"}}],
            sandbox=False,
            explain=True,
        )
        assert r["actions_executed"] == 1
        detail = r["explain"][0]["detail"]
        assert detail["deactivated"] == 2
        assert detail["snapshot"] is not None  # safety snapshot created

        # Ground truth: verify each memory by PK (avoids MatrixOne composite index bug)
        with db_factory() as db:
            rows = {
                mid: db.execute(
                    text("SELECT memory_type, is_active FROM mem_memories WHERE memory_id = :mid"),
                    {"mid": mid},
                ).fetchone()
                for mid in all_ids
            }

        sem_rows = [r for r in rows.values() if r.memory_type == "semantic"]
        proc_rows = [r for r in rows.values() if r.memory_type == "procedural"]
        assert all(r.is_active == 0 for r in sem_rows)
        assert len(proc_rows) == 1
        assert proc_rows[0].is_active == 1

        # Ground truth: edit log
        with db_factory() as db:
            log = db.execute(
                text(
                    "SELECT * FROM mem_edit_log "
                    "WHERE user_id = :uid AND operation = 'purge' "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"uid": uid},
            ).fetchone()

        assert log is not None
        assert log.snapshot_before is not None  # snapshot name recorded


# ── Tune: strategy + params persisted ─────────────────────────────────


class TestTuneE2E:
    def test_tune_sets_strategy_and_params(self, tool, uid, db_factory):
        """Tune action → verify mem_user_memory_config updated."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"tune": {"strategy": "balanced", "params": {"vector_weight": 0.6}}}],
            sandbox=False,
            explain=True,
        )
        assert r["actions_executed"] == 1
        assert r["explain"][0]["detail"]["strategy"] == "balanced"

        # Ground truth: mem_user_memory_config
        with db_factory() as db:
            cfg = db.execute(
                text("SELECT * FROM mem_user_memory_config WHERE user_id = :uid"),
                {"uid": uid},
            ).fetchone()

        assert cfg is not None
        assert cfg.strategy_key == "balanced"
        assert cfg.user_id == uid
        assert cfg.created_at is not None


# ── Sandbox: isolation + commit ───────────────────────────────────────


class TestSandboxE2E:
    def test_sandbox_does_not_write_production(self, tool, uid, db_factory):
        """Sandbox inject → production table has 0 rows."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "sandbox only"}}],
            sandbox=True,
            explain=True,
        )
        assert r["actions_executed"] == 1
        assert r["experiment_id"] is not None
        assert "hint" in r  # tells user to commit

        # Production: no rows
        with db_factory() as db:
            count = db.execute(
                text("SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid"),
                {"uid": uid},
            ).scalar()
        assert count == 0

        # Experiment record exists
        with db_factory() as db:
            exp = db.execute(
                text("SELECT * FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": r["experiment_id"]},
            ).fetchone()

        assert exp is not None
        assert exp.user_id == uid
        assert exp.status == "active"
        assert exp.branch_db is not None

    def test_sandbox_commit_writes_production(self, tool, uid, db_factory):
        """Sandbox inject → commit → data appears in production."""
        r1 = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "will be committed"}}],
            sandbox=True,
        )
        exp_id = r1["experiment_id"]

        # Commit
        r2 = _call(tool, user_id=uid, actions=[], commit=exp_id)
        assert r2["success"] is True
        assert r2["committed"] == exp_id

        # Production: row exists
        with db_factory() as db:
            count = db.execute(
                text(
                    "SELECT COUNT(*) FROM mem_memories "
                    "WHERE user_id = :uid AND content = 'will be committed'"
                ),
                {"uid": uid},
            ).scalar()
        assert count == 1


# ── Dry-run: no DB writes ────────────────────────────────────────────


class TestDryRunE2E:
    def test_dry_run_no_writes(self, tool, uid, db_factory):
        """Dry-run → no rows in any table."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "should not persist"}},
                {"purge": {"filter": {"type": "semantic"}}},
            ],
            dry_run=True,
        )
        assert r["dry_run"] is True
        assert r["actions_executed"] == 0

        with db_factory() as db:
            mem_count = db.execute(
                text("SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid"),
                {"uid": uid},
            ).scalar()
            log_count = db.execute(
                text("SELECT COUNT(*) FROM mem_edit_log WHERE user_id = :uid"),
                {"uid": uid},
            ).scalar()
            exp_count = db.execute(
                text("SELECT COUNT(*) FROM mem_experiments WHERE user_id = :uid"),
                {"uid": uid},
            ).scalar()

        assert mem_count == 0
        assert log_count == 0
        assert exp_count == 0


# ── Explain output structure ──────────────────────────────────────────


class TestExplainOutput:
    def test_explain_true_returns_detail(self, tool, uid):
        """explain=True → each result has action, success, detail keys."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "x"}}, {"inject": {"content": "y"}}],
            sandbox=False,
            explain=True,
        )
        assert "explain" in r
        for item in r["explain"]:
            assert "action" in item
            assert "success" in item
            assert "detail" in item

    def test_explain_false_returns_summary(self, tool, uid):
        """explain=False → results have action+success only, no detail."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "x"}}],
            sandbox=False,
            explain=False,
        )
        assert "results" in r
        assert "explain" not in r
        for item in r["results"]:
            assert "action" in item
            assert "success" in item
            assert "detail" not in item


# ── Error handling ────────────────────────────────────────────────────


class TestErrorHandling:
    def test_correct_nonexistent_memory(self, tool, uid):
        """Correct a non-existent memory_id → action fails, no crash."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"correct": {"memory_id": "nonexistent", "new_content": "x"}}],
            sandbox=False,
            explain=True,
        )
        assert r["actions_failed"] == 1
        assert r["explain"][0]["success"] is False
        assert "error" in r["explain"][0]

    def test_inject_missing_content(self, tool, uid):
        """Inject without content → action fails."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {}}],
            sandbox=False,
            explain=True,
        )
        assert r["actions_failed"] == 1
        assert r["explain"][0]["success"] is False


# ── Multi-action: inject + purge + verify final state ─────────────────


class TestMultiActionE2E:
    def test_inject_then_purge_final_state(self, tool, uid, db_factory):
        """Inject 3 → purge all semantic → verify exact final DB state."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "a", "type": "semantic"}},
                {"inject": {"content": "b", "type": "semantic"}},
                {"inject": {"content": "c", "type": "procedural"}},
                {"purge": {"filter": {"type": "semantic"}}},
            ],
            sandbox=False,
            explain=True,
        )
        assert r["actions_failed"] == 0
        # coalesced: batch_inject(a,b,c) + purge → 2 actions
        assert r["actions_executed"] == 2
        all_ids = r["explain"][0]["detail"]["memory_ids"]

        # Final state: verify each by PK
        with db_factory() as db:
            rows = [
                db.execute(
                    text("SELECT memory_type, content, is_active FROM mem_memories WHERE memory_id = :mid"),
                    {"mid": mid},
                ).fetchone()
                for mid in all_ids
            ]

        active = [r for r in rows if r.is_active == 1]
        inactive = [r for r in rows if r.is_active == 0]
        assert len(active) == 1
        assert active[0].memory_type == "procedural"
        assert active[0].content == "c"
        assert len(inactive) == 2
        assert all(r.memory_type == "semantic" for r in inactive)


# ── Gap 1: Batch inject field-level verification ──────────────────────


class TestBatchInjectFieldsE2E:
    def test_batch_inject_all_fields_per_row(self, tool, uid, db_factory):
        """Batch inject 3 → verify every column on each row by PK."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "batch_a", "type": "semantic", "trust": "T1"}},
                {"inject": {"content": "batch_b", "type": "procedural", "trust": "T2"}},
                {"inject": {"content": "batch_c"}},  # defaults: semantic, T2
            ],
            sandbox=False,
            explain=True,
        )
        mids = r["explain"][0]["detail"]["memory_ids"]
        assert len(mids) == 3

        with db_factory() as db:
            rows = {
                mid: db.execute(
                    text("SELECT * FROM mem_memories WHERE memory_id = :mid"),
                    {"mid": mid},
                ).fetchone()
                for mid in mids
            }

        # Row 0: batch_a
        a = rows[mids[0]]
        assert a.user_id == uid
        assert a.content == "batch_a"
        assert a.memory_type == "semantic"
        assert a.trust_tier == "T1"
        assert a.is_active == 1
        assert a.session_id is None
        assert a.superseded_by is None
        assert a.source_event_ids is not None
        assert a.observed_at is not None
        assert a.created_at is not None

        # Row 1: batch_b
        b = rows[mids[1]]
        assert b.content == "batch_b"
        assert b.memory_type == "procedural"
        assert b.trust_tier == "T2"

        # Row 2: batch_c (defaults)
        c = rows[mids[2]]
        assert c.content == "batch_c"
        assert c.memory_type == "semantic"
        assert c.trust_tier == "T2"


# ── Gap 2: Sandbox atomic rollback ───────────────────────────────────


class TestSandboxAtomicRollbackE2E:
    def test_failure_discards_experiment(self, tool, uid, db_factory):
        """Sandbox + atomic: inject ok → correct bad id → experiment discarded."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "will rollback"}},
                {"correct": {"memory_id": "nonexistent_id", "new_content": "x"}},
            ],
            sandbox=True,
            explain=True,
        )
        assert r["rolled_back"] is True
        assert r["actions_failed"] >= 1
        exp_id = r["experiment_id"]

        # Experiment should be discarded
        with db_factory() as db:
            exp = db.execute(
                text("SELECT status FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": exp_id},
            ).fetchone()

        assert exp is not None
        assert exp.status == "discarded"

        # Production: no rows
        with db_factory() as db:
            count = db.execute(
                text("SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid"),
                {"uid": uid},
            ).scalar()
        assert count == 0


# ── Gap 3: Commit → experiment status change ─────────────────────────


class TestCommitStatusE2E:
    def test_commit_changes_experiment_status(self, tool, uid, db_factory):
        """Sandbox → commit → experiment status becomes 'committed'."""
        r1 = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "to commit"}}],
            sandbox=True,
        )
        exp_id = r1["experiment_id"]

        # Before commit: active
        with db_factory() as db:
            before = db.execute(
                text("SELECT status, committed_at FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": exp_id},
            ).fetchone()
        assert before.status == "active"
        assert before.committed_at is None

        # Commit
        r2 = _call(tool, user_id=uid, actions=[], commit=exp_id)
        assert r2["success"] is True

        # After commit: committed + timestamp set
        with db_factory() as db:
            after = db.execute(
                text("SELECT status, committed_at FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": exp_id},
            ).fetchone()
        assert after.status == "committed"
        assert after.committed_at is not None


# ── Gap 4: Tune → params_json value verification ─────────────────────


class TestTuneParamsE2E:
    def test_tune_params_json_persisted(self, tool, uid, db_factory):
        """Tune with params → verify params_json content in DB."""
        params = {"vector_weight": 0.7, "keyword_weight": 0.3}
        r = _call(
            tool,
            user_id=uid,
            actions=[{"tune": {"strategy": "balanced", "params": params}}],
            sandbox=False,
            explain=True,
        )
        assert r["actions_executed"] == 1
        returned_params = r["explain"][0]["detail"]["params"]
        assert returned_params["vector_weight"] == 0.7
        assert returned_params["keyword_weight"] == 0.3

        # Ground truth: DB
        with db_factory() as db:
            cfg = db.execute(
                text("SELECT strategy_key, params_json FROM mem_user_memory_config WHERE user_id = :uid"),
                {"uid": uid},
            ).fetchone()

        assert cfg.strategy_key == "balanced"
        # params_json may be string or dict depending on driver
        pj = cfg.params_json
        if isinstance(pj, str):
            pj = json.loads(pj)
        assert pj["vector_weight"] == 0.7
        assert pj["keyword_weight"] == 0.3
