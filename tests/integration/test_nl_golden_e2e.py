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

@pytest.fixture
def programmer(db_factory):
    from core.memory.canonical_storage import CanonicalStorage
    from core.memory.editor import MemoryEditor
    from core.memory.experiment import MemoryExperimentManager

    storage = CanonicalStorage(db_factory)
    editor = MemoryEditor(storage, db_factory)
    experiments = MemoryExperimentManager(db_factory, source_db=_TEST_DB)
    return MemoryProgrammer(editor, experiments, db_factory)


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


class TestEdgeToolForceSandbox:
    """EdgeTool defaults to sandbox=False — writes go directly to production."""

    def test_edge_tool_defaults_to_no_sandbox(self, db_factory):
        """MemoryProgramTool._default_sandbox is False by default."""
        from cli.tools.memory_program import MemoryProgramTool

        tool = MemoryProgramTool()
        assert tool._default_sandbox is False

    def test_edge_tool_sandbox_true_creates_experiment(self, db_factory):
        """Explicit sandbox=True still creates experiment branch."""
        import asyncio

        from cli.tools.memory_program import MemoryProgramTool

        tool = MemoryProgramTool()
        user_id = f"edgesandbox{generate_id()}"

        raw = asyncio.run(tool.execute(
            user_id=user_id,
            actions=[{"inject": {"content": "edge sandbox test", "type": "semantic"}}],
            sandbox=True,
        ))
        result = json.loads(raw)
        assert result.get("experiment_id") is not None
        assert "hint" in result


class TestBatchInjectEmbedding:
    """batch_inject must use embed_client for batch embedding."""

    def test_batch_inject_calls_embed_batch(self, db_factory):
        """embed_client.embed_batch() is called once, embeddings written to DB."""
        from unittest.mock import MagicMock

        from core.memory.canonical_storage import CanonicalStorage
        from core.memory.editor import MemoryEditor

        dim = 384  # must match DB vector column dimension
        mock_client = MagicMock()
        mock_client.embed_batch.return_value = [[0.1] * dim, [0.2] * dim]

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory, embed_client=mock_client)

        user_id = f"emb-batch-{generate_id()}"
        specs = [
            {"content": "fact one", "type": "semantic"},
            {"content": "fact two", "type": "semantic"},
        ]
        stored = editor.batch_inject(user_id, specs)

        # embed_batch called exactly once with both texts
        mock_client.embed_batch.assert_called_once_with(["fact one", "fact two"])

        # Embeddings persisted to DB
        with db_factory() as db:
            for mem in stored:
                row = db.execute(
                    text("SELECT embedding FROM mem_memories WHERE memory_id = :mid"),
                    {"mid": mem.memory_id},
                ).fetchone()
                assert row is not None
                assert row.embedding is not None

    def test_batch_inject_without_embed_client_still_works(self, db_factory):
        """No embed_client → memories created without embeddings."""
        from core.memory.canonical_storage import CanonicalStorage
        from core.memory.editor import MemoryEditor

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory, embed_client=None)

        user_id = f"emb-none-{generate_id()}"
        stored = editor.batch_inject(user_id, [{"content": "no embedding"}])

        assert len(stored) == 1
        with db_factory() as db:
            row = db.execute(
                text("SELECT embedding FROM mem_memories WHERE memory_id = :mid"),
                {"mid": stored[0].memory_id},
            ).fetchone()
            assert row is not None
            assert row.embedding is None

    def test_create_editor_has_embed_client(self, db_factory):
        """create_editor resolves embed_client when embedding is configured."""
        from core.memory.factory import create_editor

        editor = create_editor(db_factory)
        # embed_client may be None if embedding provider not configured in test env,
        # but the attribute must exist (not reaching into storage._embed_fn)
        assert hasattr(editor, "_embed_client")


class TestSessionIdPropagation:
    """session_id must flow from execute() through to mem_memories rows."""

    def test_inject_persists_session_id(self, db_factory):
        """Single inject with session_id → session_id in DB row."""
        from core.memory.canonical_storage import CanonicalStorage
        from core.memory.editor import MemoryEditor
        from core.memory.experiment import MemoryExperimentManager

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)
        experiments = MemoryExperimentManager(db_factory, source_db=_TEST_DB)
        programmer = MemoryProgrammer(editor, experiments, db_factory)

        user_id = f"sess-inj-{generate_id()}"
        session_id = f"sess-{generate_id()}"
        script = [{"inject": {"content": "session test fact", "type": "semantic"}}]

        result = programmer.execute(user_id, script, sandbox=False, session_id=session_id)
        assert result.actions_executed == 1

        mid = result.results[0].detail["memory_id"]
        with db_factory() as db:
            row = db.execute(
                text("SELECT session_id FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            ).fetchone()
            assert row is not None
            assert row.session_id == session_id

    def test_batch_inject_persists_session_id(self, db_factory):
        """Batch inject with session_id → all rows have session_id."""
        from core.memory.canonical_storage import CanonicalStorage
        from core.memory.editor import MemoryEditor
        from core.memory.experiment import MemoryExperimentManager

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)
        experiments = MemoryExperimentManager(db_factory, source_db=_TEST_DB)
        programmer = MemoryProgrammer(editor, experiments, db_factory)

        user_id = f"sess-bat-{generate_id()}"
        session_id = f"sess-{generate_id()}"
        script = [
            {"inject": {"content": "batch sess fact 1"}},
            {"inject": {"content": "batch sess fact 2"}},
            {"inject": {"content": "batch sess fact 3"}},
        ]

        result = programmer.execute(user_id, script, sandbox=False, session_id=session_id)
        assert result.actions_executed == 1  # coalesced into 1 batch

        mids = result.results[0].detail["memory_ids"]
        assert len(mids) == 3

        with db_factory() as db:
            for mid in mids:
                row = db.execute(
                    text("SELECT session_id FROM mem_memories WHERE memory_id = :mid"),
                    {"mid": mid},
                ).fetchone()
                assert row is not None, f"memory {mid} not found"
                assert row.session_id == session_id, f"memory {mid} has wrong session_id"

    def test_no_session_id_defaults_to_null(self, db_factory):
        """Without session_id → DB row has NULL session_id."""
        from core.memory.canonical_storage import CanonicalStorage
        from core.memory.editor import MemoryEditor
        from core.memory.experiment import MemoryExperimentManager

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)
        experiments = MemoryExperimentManager(db_factory, source_db=_TEST_DB)
        programmer = MemoryProgrammer(editor, experiments, db_factory)

        user_id = f"sess-nil-{generate_id()}"
        script = [{"inject": {"content": "no session fact"}}]

        result = programmer.execute(user_id, script, sandbox=False)
        mid = result.results[0].detail["memory_id"]

        with db_factory() as db:
            row = db.execute(
                text("SELECT session_id FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            ).fetchone()
            assert row is not None
            assert row.session_id is None

    def test_edge_tool_passes_session_id(self, db_factory):
        """MemoryProgramTool with session_info → session_id reaches DB."""
        import asyncio

        from cli.tools.memory_program import MemoryProgramTool

        user_id = f"edge-sess-{generate_id()}"
        session_id = f"sess-{generate_id()}"
        tool = MemoryProgramTool(session_info={"session_id": session_id})

        raw = asyncio.run(tool.execute(
            user_id=user_id,
            actions=[{"inject": {"content": "edge session test"}}],
            sandbox=False,
        ))
        result = json.loads(raw)
        assert result.get("actions_failed", 1) == 0

        with db_factory() as db:
            row = db.execute(
                text("SELECT session_id FROM mem_memories WHERE user_id = :uid AND is_active"),
                {"uid": user_id},
            ).fetchone()
            assert row is not None
            assert row.session_id == session_id
