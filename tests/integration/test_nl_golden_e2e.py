"""Golden-session tests for nl_to_script: recorded DeepSeek outputs → parse → execute → DB.

Golden outputs recorded from deepseek-chat (2026-03-08, temperature=0).
No real LLM calls — tests replay recorded responses through parse_script
and MemoryProgrammer.execute to verify the full pipeline.
"""

import json
import os

import pytest
from sqlalchemy import text

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")
_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")

from core.memory.programmer import MemoryProgrammer, parse_script  # noqa: E402
from core.utils.id_generator import generate_id  # noqa: E402
from tests.conftest import TEST_EMBEDDING_DIM  # noqa: E402

# ── Golden LLM outputs (deepseek-chat, 2026-03-08) ──────────────────

GOLDEN = {
    "inject_preference": {
        "input": "Remember that I prefer Python over Java for backend development",
        "raw": (
            "version: 1\nactions:\n  - inject:\n      user_id: test-user\n"
            "      memory_type: semantic\n"
            "      content: User prefers Python over Java for backend development.\n"
            "      trust_tier: T2"
        ),
    },
    "purge_working": {
        "input": "Delete all my working memories",
        "raw": (
            "version: 1\nactions:\n  - purge:\n      filter:\n"
            "        user_id: test-user\n        memory_type: working"
        ),
    },
    "multi_inject_purge": {
        "input": "Remember I use neovim as my editor, and delete all my procedural memories",
        "raw": (
            "version: 1\nactions:\n  - inject:\n      user_id: test-user\n"
            "      memory_type: procedural\n"
            "      content: User uses neovim as their editor.\n"
            "      trust_tier: T2\n"
            "  - purge:\n      filter:\n        memory_type: procedural"
        ),
    },
    "inject_verified_t1": {
        "input": "I am certain that our API rate limit is 1000 requests per minute",
        "raw": (
            "version: 1\nactions:\n  - inject:\n      user_id: test-user\n"
            "      memory_type: semantic\n"
            "      content: API rate limit is 1000 requests per minute\n"
            "      trust_tier: T1"
        ),
    },
    "tune_explicit": {
        "input": "Tune my memory config: set strategy to recency_weighted",
        "raw": (
            "version: 1\nactions:\n  - tune:\n      user_id: test-user\n"
            "      strategy: recency_weighted\n"
            "      vector_weight: 0.3\n      keyword_weight: 0.7"
        ),
    },
}


# ── Fixtures ─────────────────────────────────────────────────────────

@pytest.fixture(autouse=True)
def _setup_memoria_env():
    """Set Memoria environment variables from test configuration."""
    os.environ["MEMORIA_BASE_URL"] = os.environ.get("TEST_MEMORIA_BASE_URL", "http://localhost:8100")
    os.environ["MEMORIA_MASTER_KEY"] = os.environ.get("TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
    os.environ["MEMORIA_API_KEY"] = os.environ.get("TEST_MEMORIA_API_KEY", "")
    yield


@pytest.fixture
def programmer(db_factory):
    """Create programmer with test Memoria configuration."""
    from core.memory.factory import create_editor
    from core.memory.programmer import MemoryProgrammer

    editor = create_editor(db_factory)
    return MemoryProgrammer(editor, db_factory)


# ── Parse-only tests (no DB) ────────────────────────────────────────


class TestGoldenParse:
    """Verify recorded LLM outputs parse into correct action structures."""

    def test_inject_preference(self):
        actions = parse_script(GOLDEN["inject_preference"]["raw"])
        assert len(actions) == 1
        inj = actions[0]["inject"]
        assert inj["type"] == "semantic"
        assert "Python" in inj["content"]
        assert inj["trust"] == "T2"

    def test_purge_working(self):
        actions = parse_script(GOLDEN["purge_working"]["raw"])
        assert len(actions) == 1
        assert actions[0]["purge"]["filter"]["type"] == "working"

    def test_multi_action(self):
        actions = parse_script(GOLDEN["multi_inject_purge"]["raw"])
        assert len(actions) == 2
        assert "inject" in actions[0]
        assert "purge" in actions[1]
        assert "neovim" in actions[0]["inject"]["content"].lower()

    def test_verified_gets_t1(self):
        actions = parse_script(GOLDEN["inject_verified_t1"]["raw"])
        assert actions[0]["inject"]["trust"] == "T1"

    def test_tune_action(self):
        actions = parse_script(GOLDEN["tune_explicit"]["raw"])
        assert len(actions) == 1
        tune = actions[0]["tune"]
        assert tune["strategy"] == "recency_weighted"


# ── Execute → DB tests ──────────────────────────────────────────────


class TestGoldenExecute:
    """Parse golden output → execute → verify DB ground truth."""

    def test_inject_persists(self, programmer, db_factory):
        user_id = f"golden-{generate_id()}"
        actions = parse_script(GOLDEN["inject_preference"]["raw"])
        actions[0]["inject"]["user_id"] = user_id

        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed == 1
        assert result.actions_failed == 0

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT content, memory_type, is_active "
                    "FROM mem_memories WHERE user_id = :uid AND is_active != 0"
                ),
                {"uid": user_id},
            ).fetchone()
            assert row is not None
            assert "Python" in row.content
            assert row.memory_type == "semantic"
            assert row.is_active == 1

    def test_inject_t1_trust(self, programmer, db_factory):
        user_id = f"golden-{generate_id()}"
        actions = parse_script(GOLDEN["inject_verified_t1"]["raw"])
        actions[0]["inject"]["user_id"] = user_id

        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed == 1

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT trust_tier FROM mem_memories "
                    "WHERE user_id = :uid AND is_active != 0"
                ),
                {"uid": user_id},
            ).fetchone()
            assert row is not None
            assert row.trust_tier == "T1"

    def test_multi_action_executes_all(self, programmer, db_factory):
        """Inject + purge: both actions execute successfully."""
        user_id = f"golden-{generate_id()}"
        actions = parse_script(GOLDEN["multi_inject_purge"]["raw"])
        actions[0]["inject"]["user_id"] = user_id
        actions[1]["purge"]["filter"]["user_id"] = user_id

        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed == 2
        assert result.actions_failed == 0

    def test_tune_persists_config(self, programmer, db_factory):
        user_id = f"golden-{generate_id()}"
        actions = parse_script(GOLDEN["tune_explicit"]["raw"])
        actions[0]["tune"]["user_id"] = user_id

        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed == 1

        with db_factory() as db:
            row = db.execute(
                text("SELECT strategy_key FROM mem_user_memory_config WHERE user_id = :uid"),
                {"uid": user_id},
            ).fetchone()
            assert row is not None
            assert row.strategy_key == "recency_weighted"


class TestSnapshotNameSafe:
    """Snapshot names must pass validate_identifier (alphanumeric + underscore only)."""

    def test_purge_snapshot_with_hyphenated_user_id(self, programmer, db_factory):
        """User IDs with hyphens must not break snapshot creation."""
        user_id = f"snap-test-{generate_id()}"
        actions = parse_script(GOLDEN["purge_working"]["raw"])
        actions[0]["purge"]["filter"]["user_id"] = user_id

        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed == 1
        purge_detail = result.results[0].detail
        snapshot_name = purge_detail["snapshot"]
        assert snapshot_name is not None
        assert "-" not in snapshot_name

        # Verify snapshot actually exists in DB
        with db_factory() as db:
            row = db.execute(
                text("SHOW SNAPSHOTS WHERE SNAPSHOT_NAME = :n"),
                {"n": snapshot_name},
            ).fetchone()
            assert row is not None


class TestBatchInjectAudit:
    """batch_inject must go through editor — audit log + source_event_ids."""

    def test_batch_inject_has_source_event_ids(self, programmer, db_factory):
        """Batch-injected memories must have non-empty source_event_ids."""
        user_id = f"golden-{generate_id()}"
        # 3 consecutive injects → coalesced into batch
        actions = parse_script([
            {"inject": {"content": "fact A", "type": "semantic"}},
            {"inject": {"content": "fact B", "type": "semantic"}},
            {"inject": {"content": "fact C", "type": "semantic"}},
        ])
        result = programmer.execute(user_id, actions, sandbox=False)
        assert result.actions_executed >= 1

        with db_factory() as db:
            rows = db.execute(
                text(
                    "SELECT source_event_ids FROM mem_memories "
                    "WHERE user_id = :uid AND is_active != 0"
                ),
                {"uid": user_id},
            ).fetchall()
            assert len(rows) == 3
            for row in rows:
                # Must not be empty '[]'
                assert row.source_event_ids != "[]"

    def test_batch_inject_has_edit_log(self, programmer, db_factory):
        """Batch-injected memories must produce edit_log entries."""
        user_id = f"golden-{generate_id()}"
        actions = parse_script([
            {"inject": {"content": "logged A"}},
            {"inject": {"content": "logged B"}},
        ])
        programmer.execute(user_id, actions, sandbox=False)

        with db_factory() as db:
            ops = [r.operation for r in db.execute(
                text("SELECT operation FROM mem_edit_log WHERE user_id = :uid"),
                {"uid": user_id},
            ).fetchall()]
            assert "inject" in ops
            assert "program" in ops


class TestProgramAuditLog:
    """Every execute() must write a program-level audit entry."""

    def test_program_audit_on_success(self, programmer, db_factory):
        user_id = f"golden-{generate_id()}"
        actions = parse_script(GOLDEN["inject_preference"]["raw"])
        actions[0]["inject"]["user_id"] = user_id
        programmer.execute(user_id, actions, sandbox=False)

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT operation, target_ids, reason FROM mem_edit_log "
                    "WHERE user_id = :uid AND operation = 'program'"
                ),
                {"uid": user_id},
            ).fetchone()
            assert row is not None
            assert row.reason == "unnamed"  # default program_name
            # target_ids should contain the memory_id
            assert row.target_ids != "[]"


class TestEditorHasIndexManager:
    """create_editor wires up index_manager based on user's strategy."""

    def test_activation_strategy_has_index_manager(self, db_factory):
        """activation:v1 strategy needs graph index → index_manager must be set."""
        from core.memory.factory import create_editor, set_user_strategy

        user_id = f"idx-test-{generate_id()}"
        set_user_strategy(db_factory, user_id, "activation:v1")
        editor = create_editor(db_factory, user_id=user_id)
        assert editor._index_manager is not None

    def test_activation_inject_creates_graph_node(self, db_factory):
        """Inject via activation editor → graph_nodes table must have a row."""
        from core.memory.factory import create_editor, set_user_strategy
        from core.memory.types import MemoryType

        user_id = f"idx-e2e-{generate_id()}"
        set_user_strategy(db_factory, user_id, "activation:v1")
        editor = create_editor(db_factory, user_id=user_id)

        editor.inject(user_id, "Graph index test fact", memory_type=MemoryType.SEMANTIC)

        with db_factory() as db:
            row = db.execute(
                text("SELECT node_id FROM memory_graph_nodes WHERE user_id = :uid"),
                {"uid": user_id},
            ).fetchone()
            assert row is not None, "graph_nodes should have a row after inject with activation strategy"

    def test_vector_strategy_has_no_index_manager(self, db_factory, monkeypatch):
        """vector:v1 doesn't need graph index → index_manager is None."""
        from core.memory.factory import create_editor

        monkeypatch.setenv("MEM_RETRIEVAL_STRATEGY", "vector:v1")
        editor = create_editor(db_factory, user_id="any-user")
        assert editor._index_manager is None

    def test_create_editor_without_user(self, db_factory):
        from core.memory.factory import create_editor

        editor = create_editor(db_factory)
        assert editor._index_manager is None
