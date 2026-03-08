"""Integration tests for Phase 5: MemoryProgrammer.

Tests:
- Script parsing (YAML, dict, list formats)
- Validation (malformed scripts, unknown actions, missing fields)
- Dry-run mode
- inject/correct/purge action execution via real MemoryEditor
- Sandboxed execution (experiment created)
- Non-sandboxed execution
"""

import os
import uuid

import pytest
from sqlalchemy import text

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")
_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")

from core.memory.programmer import (  # noqa: E402
    InvalidScriptError,
    MemoryProgrammer,
    parse_script,
)

# ── Script Parsing (no DB needed) ────────────────────────────────────


class TestParseScript:
    def test_parse_dict_with_actions_key(self):
        raw = {"version": 1, "actions": [{"inject": {"content": "hello"}}]}
        actions = parse_script(raw)
        assert len(actions) == 1
        assert "inject" in actions[0]

    def test_parse_list_of_actions(self):
        raw = [{"inject": {"content": "a"}}, {"purge": {"filter": {}}}]
        actions = parse_script(raw)
        assert len(actions) == 2

    def test_parse_single_action_dict(self):
        raw = {"inject": {"content": "hello"}}
        actions = parse_script(raw)
        assert len(actions) == 1

    def test_parse_yaml_string(self):
        yaml_str = """
version: 1
actions:
  - inject:
      content: "hello from yaml"
  - purge:
      filter:
        type: semantic
"""
        actions = parse_script(yaml_str)
        assert len(actions) == 2
        assert actions[0]["inject"]["content"] == "hello from yaml"

    def test_reject_empty_actions(self):
        with pytest.raises(InvalidScriptError, match="no actions"):
            parse_script({"version": 1, "actions": []})

    def test_reject_wrong_version(self):
        with pytest.raises(InvalidScriptError, match="Unsupported script version"):
            parse_script({"version": 99, "actions": [{"inject": {"content": "x"}}]})

    def test_reject_unknown_action_key(self):
        with pytest.raises(InvalidScriptError, match="no recognized action key"):
            parse_script([{"unknown_op": {"foo": "bar"}}])

    def test_reject_multiple_action_keys(self):
        with pytest.raises(InvalidScriptError, match="multiple action keys"):
            parse_script([{"inject": {"content": "x"}, "purge": {"filter": {}}}])

    def test_reject_non_dict_action(self):
        with pytest.raises(InvalidScriptError, match="not a dict"):
            parse_script(["not a dict"])

    def test_reject_invalid_yaml(self):
        with pytest.raises(InvalidScriptError, match="Invalid YAML"):
            parse_script("{{{{not valid yaml")

    def test_reject_wrong_type(self):
        with pytest.raises(InvalidScriptError, match="Expected dict"):
            parse_script(42)  # type: ignore[arg-type]


# ── Dry Run (no DB needed) ───────────────────────────────────────────


class TestDryRun:
    def test_dry_run_returns_action_types(self):
        """Dry run parses and validates but doesn't execute."""
        from unittest.mock import MagicMock

        programmer = MemoryProgrammer(
            editor=MagicMock(),
            experiments=MagicMock(),
            db_factory=MagicMock(),
        )
        result = programmer.execute(
            "user1",
            [{"inject": {"content": "a"}}, {"purge": {"filter": {}}}],
            dry_run=True,
        )
        assert result.dry_run is True
        assert result.actions_executed == 0
        assert len(result.results) == 2
        assert result.results[0].action_type == "inject"
        assert result.results[1].action_type == "purge"
        assert all(r.detail.get("dry_run") for r in result.results)


# ── Real Execution (requires DB) ─────────────────────────────────────


from api.database import SessionLocal  # noqa: E402
from core.memory.experiment import MemoryExperimentManager  # noqa: E402


@pytest.fixture()
def db_factory():
    return SessionLocal


@pytest.fixture()
def editor(db_factory):
    from core.memory.canonical_storage import CanonicalStorage
    from core.memory.editor import MemoryEditor

    storage = CanonicalStorage(db_factory)
    return MemoryEditor(storage, db_factory)


@pytest.fixture()
def experiments(db_factory):
    return MemoryExperimentManager(db_factory, source_db=_TEST_DB)


@pytest.fixture()
def programmer(editor, experiments, db_factory):
    return MemoryProgrammer(editor, experiments, db_factory)


@pytest.fixture(autouse=True)
def _cleanup(db_factory):
    """Clean up test data after each test."""
    yield
    with db_factory() as db:
        # Clean experiments
        rows = db.execute(
            text(
                "SELECT branch_db, base_snapshot FROM mem_experiments "
                "WHERE user_id LIKE 'test_prog_%'"
            )
        ).fetchall()
        branch_dbs = [r.branch_db for r in rows]
        snap_names = [r.base_snapshot for r in rows if r.base_snapshot]
        db.execute(text("DELETE FROM mem_experiments WHERE user_id LIKE 'test_prog_%'"))
        # Clean memories
        db.execute(text("DELETE FROM mem_memories WHERE user_id LIKE 'test_prog_%'"))
        # Clean edit log
        db.execute(text("DELETE FROM mem_edit_log WHERE user_id LIKE 'test_prog_%'"))
        # Clean user config
        db.execute(text("DELETE FROM mem_user_memory_config WHERE user_id LIKE 'test_prog_%'"))
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


class TestInjectAction:
    def test_inject_creates_memory(self, programmer, db_factory):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"inject": {"content": "Python prefers spaces over tabs", "type": "semantic"}}],
            sandbox=False,
        )
        assert result.actions_executed == 1
        assert result.actions_failed == 0
        assert result.results[0].success is True
        memory_id = result.results[0].detail["memory_id"]

        # Verify in DB
        with db_factory() as db:
            row = db.execute(
                text("SELECT content, memory_type, is_active FROM mem_memories WHERE memory_id = :mid"),
                {"mid": memory_id},
            ).fetchone()
            assert row is not None
            assert row.content == "Python prefers spaces over tabs"
            assert row.memory_type == "semantic"
            assert row.is_active == 1

    def test_inject_missing_content_fails(self, programmer):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"inject": {"type": "semantic"}}],
            sandbox=False,
        )
        assert result.actions_failed == 1
        assert result.results[0].success is False
        assert "content" in result.results[0].error


class TestCorrectAction:
    def test_correct_supersedes_memory(self, programmer, db_factory):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        # First inject
        r1 = programmer.execute(
            uid,
            [{"inject": {"content": "Earth is flat", "type": "semantic"}}],
            sandbox=False,
        )
        old_id = r1.results[0].detail["memory_id"]

        # Then correct
        r2 = programmer.execute(
            uid,
            [{"correct": {"memory_id": old_id, "new_content": "Earth is round"}}],
            sandbox=False,
        )
        assert r2.actions_executed == 1
        new_id = r2.results[0].detail["new_id"]
        assert new_id != old_id

        # Old memory deactivated, new one active
        with db_factory() as db:
            old = db.execute(
                text("SELECT is_active, superseded_by FROM mem_memories WHERE memory_id = :mid"),
                {"mid": old_id},
            ).fetchone()
            assert old.is_active == 0
            assert old.superseded_by == new_id

            new = db.execute(
                text("SELECT content, is_active FROM mem_memories WHERE memory_id = :mid"),
                {"mid": new_id},
            ).fetchone()
            assert new.content == "Earth is round"
            assert new.is_active == 1

    def test_correct_missing_fields_fails(self, programmer):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"correct": {"memory_id": "nonexistent"}}],
            sandbox=False,
        )
        assert result.results[0].success is False
        assert "new_content" in result.results[0].error


class TestPurgeAction:
    def test_purge_by_type(self, programmer, db_factory):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        # Inject two memories
        programmer.execute(
            uid,
            [
                {"inject": {"content": "fact 1", "type": "semantic"}},
                {"inject": {"content": "fact 2", "type": "semantic"}},
            ],
            sandbox=False,
        )

        # Purge all semantic
        result = programmer.execute(
            uid,
            [{"purge": {"filter": {"type": "semantic"}}}],
            sandbox=False,
        )
        assert result.actions_executed == 1
        assert result.results[0].detail["deactivated"] == 2

        # Verify in DB
        with db_factory() as db:
            active = db.execute(
                text(
                    "SELECT COUNT(*) AS cnt FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1"
                ),
                {"uid": uid},
            ).scalar()
            assert active == 0


class TestSandboxExecution:
    def test_sandbox_creates_experiment(self, programmer, db_factory):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"inject": {"content": "sandboxed fact"}}],
            sandbox=True,
            program_name="test_sandbox",
        )
        assert result.experiment_id is not None

        # Verify experiment exists in DB
        with db_factory() as db:
            row = db.execute(
                text("SELECT name, status FROM mem_experiments WHERE experiment_id = :eid"),
                {"eid": result.experiment_id},
            ).fetchone()
            assert row is not None
            assert row.name == "prog_test_sandbox"
            assert row.status == "active"

    def test_sandbox_inject_isolated(self, programmer, db_factory):
        """Sandbox inject writes to branch DB, not production."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"

        # Count production memories before
        with db_factory() as db:
            before = db.execute(
                text(
                    "SELECT COUNT(*) AS cnt FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1"
                ),
                {"uid": uid},
            ).scalar()

        result = programmer.execute(
            uid,
            [{"inject": {"content": "branch-only fact", "type": "semantic"}}],
            sandbox=True,
            program_name="isolation_test",
        )
        assert result.actions_executed == 1
        assert result.experiment_id is not None

        # Production should be unchanged
        with db_factory() as db:
            after = db.execute(
                text(
                    "SELECT COUNT(*) AS cnt FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1"
                ),
                {"uid": uid},
            ).scalar()
        assert after == before

    def test_sandbox_commit_applies_changes(self, programmer, experiments, db_factory):
        """Full workflow: sandbox inject → commit → data appears in production."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"

        # Sandbox inject
        result = programmer.execute(
            uid,
            [{"inject": {"content": "will be committed", "type": "semantic"}}],
            sandbox=True,
            program_name="commit_test",
        )
        assert result.experiment_id is not None

        # Not in production yet
        with db_factory() as db:
            prod_before = db.execute(
                text(
                    "SELECT COUNT(*) AS cnt FROM mem_memories "
                    "WHERE user_id = :uid AND content = 'will be committed'"
                ),
                {"uid": uid},
            ).scalar()
        assert prod_before == 0

        # Commit
        experiments.commit(result.experiment_id)

        # Now in production
        with db_factory() as db:
            prod_after = db.execute(
                text(
                    "SELECT content, is_active, memory_type FROM mem_memories "
                    "WHERE user_id = :uid AND content = 'will be committed'"
                ),
                {"uid": uid},
            ).fetchone()
        assert prod_after is not None
        assert prod_after.is_active == 1
        assert prod_after.memory_type == "semantic"

        # Experiment status is committed
        info = experiments.get(result.experiment_id)
        assert info.status == "committed"


class TestTuneAction:
    def test_tune_sets_strategy_and_params(self, programmer, db_factory):
        """Tune action sets user strategy and persists validated params."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"tune": {
                "strategy": "vector:v1",
                "params": {"semantic_weight": 0.6, "temporal_weight": 0.2},
            }}],
            sandbox=False,
        )
        assert result.actions_executed == 1
        assert result.results[0].success is True
        assert result.results[0].detail["strategy"] == "vector:v1"
        # Validated params include defaults for unspecified fields
        validated = result.results[0].detail["params"]
        assert validated["semantic_weight"] == 0.6
        assert validated["temporal_weight"] == 0.2

        # Verify strategy set in DB (use ORM to get proper JSON deserialization)
        from api.models.memory_config import MemoryUserConfig

        with db_factory() as db:
            row = db.query(MemoryUserConfig).filter_by(user_id=uid).first()
            assert row is not None
            assert row.strategy_key == "vector:v1"
            assert row.params_json is not None
            assert row.params_json["semantic_weight"] == 0.6

    def test_tune_missing_strategy_fails(self, programmer):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [{"tune": {"params": {"semantic_weight": 0.5}}}],
            sandbox=False,
        )
        assert result.results[0].success is False
        assert "strategy" in result.results[0].error


class TestMultiActionScript:
    def test_multi_action_yaml_workflow(self, programmer, db_factory):
        """Full workflow: inject two memories, correct one, purge the other."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"

        # Step 1: inject two (coalesced into one batch action)
        r1 = programmer.execute(
            uid,
            [
                {"inject": {"content": "memory A", "type": "semantic"}},
                {"inject": {"content": "memory B", "type": "procedural"}},
            ],
            sandbox=False,
        )
        assert r1.actions_failed == 0
        # Batch coalesces consecutive injects: 1 batch action, 2 memories
        assert r1.results[0].detail["count"] == 2
        id_a = r1.results[0].detail["memory_ids"][0]

        # Step 2: correct A and purge procedural — as YAML
        yaml_script = f"""
version: 1
actions:
  - correct:
      memory_id: "{id_a}"
      new_content: "memory A corrected"
      reason: "was wrong"
  - purge:
      filter:
        type: procedural
      reason: "cleanup"
"""
        r2 = programmer.execute(uid, yaml_script, sandbox=False)
        assert r2.actions_executed == 2
        assert r2.actions_failed == 0

        # Verify final state
        with db_factory() as db:
            rows = db.execute(
                text(
                    "SELECT memory_id, content, is_active, memory_type "
                    "FROM mem_memories WHERE user_id = :uid ORDER BY created_at"
                ),
                {"uid": uid},
            ).fetchall()

        active = [r for r in rows if r.is_active == 1]
        assert len(active) == 1
        assert active[0].content == "memory A corrected"

    def test_partial_failure_continues(self, programmer):
        """With atomic=False, if one action fails, others still execute."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [
                {"inject": {"content": "good one"}},
                {"correct": {"memory_id": "nonexistent_id", "new_content": "x"}},
                {"inject": {"content": "another good one"}},
            ],
            sandbox=False,
            atomic=False,
        )
        assert result.actions_executed == 2
        assert result.actions_failed == 1
        assert result.results[0].success is True
        assert result.results[1].success is False
        assert result.results[2].success is True


class TestEditAuditTrail:
    def test_inject_logged_in_edit_log(self, programmer, db_factory):
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        programmer.execute(
            uid,
            [{"inject": {"content": "audited fact"}}],
            sandbox=False,
        )
        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT operation, user_id FROM mem_edit_log "
                    "WHERE user_id = :uid ORDER BY created_at DESC LIMIT 1"
                ),
                {"uid": uid},
            ).fetchone()
            assert row is not None
            assert row.operation == "inject"
            assert row.user_id == uid


class TestAtomicRollback:
    def test_sandbox_atomic_discards_on_failure(self, programmer, experiments, db_factory):
        """atomic=True (default): failure discards the experiment."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [
                {"inject": {"content": "will be rolled back"}},
                {"correct": {"memory_id": "nonexistent", "new_content": "x"}},
            ],
            sandbox=True,
            program_name="atomic_rollback",
        )
        assert result.rolled_back is True
        assert result.actions_failed == 1

        # Experiment should be discarded
        info = experiments.get(result.experiment_id)
        assert info.status == "discarded"

    def test_atomic_stops_on_first_failure(self, programmer):
        """atomic=True stops executing after first failure."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [
                {"correct": {"memory_id": "bad_id", "new_content": "x"}},
                {"inject": {"content": "should not run"}},
            ],
            sandbox=False,
            atomic=True,
        )
        # Only 1 result — second action was skipped
        assert len(result.results) == 1
        assert result.results[0].success is False

    def test_non_atomic_continues_after_failure(self, programmer):
        """atomic=False runs all actions regardless of failures."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [
                {"correct": {"memory_id": "bad_id", "new_content": "x"}},
                {"inject": {"content": "runs anyway"}},
            ],
            sandbox=False,
            atomic=False,
        )
        assert len(result.results) == 2
        assert result.results[0].success is False
        assert result.results[1].success is True
        assert result.rolled_back is False


class TestBatchInject:
    def test_consecutive_injects_batched(self, programmer, db_factory):
        """Multiple consecutive injects are coalesced into one batch INSERT."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        n = 20
        actions = [{"inject": {"content": f"fact {i}", "type": "semantic"}} for i in range(n)]
        result = programmer.execute(uid, actions, sandbox=False)

        # All coalesced into 1 batch action
        assert len(result.results) == 1
        assert result.results[0].detail["count"] == n
        assert result.actions_failed == 0

        # All 20 in DB
        with db_factory() as db:
            count = db.execute(
                text(
                    "SELECT COUNT(*) FROM mem_memories "
                    "WHERE user_id = :uid AND is_active = 1"
                ),
                {"uid": uid},
            ).scalar()
        assert count == n

    def test_non_consecutive_injects_not_batched(self, programmer):
        """Injects separated by other actions are not coalesced."""
        uid = f"test_prog_{uuid.uuid4().hex[:8]}"
        result = programmer.execute(
            uid,
            [
                {"inject": {"content": "a"}},
                {"purge": {"filter": {"type": "working"}}},
                {"inject": {"content": "b"}},
            ],
            sandbox=False,
        )
        # 3 separate actions: inject, purge, inject
        assert len(result.results) == 3
