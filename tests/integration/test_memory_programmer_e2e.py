"""Integration tests for MemoryProgrammer with Memoria backend.

Tests:
- Script parsing (YAML, dict, list formats)
- Validation (malformed scripts, unknown actions, missing fields)
- Dry-run mode
- inject/correct/purge/tune action execution via Memoria
"""

import os

import pytest

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")
_TEST_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")

from core.memory.programmer import (  # noqa: E402
    InvalidScriptError,
    MemoryProgrammer,
    parse_script,
    ProgramTimeoutError,
)
from core.utils.id_generator import generate_id  # noqa: E402

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


# ── Real Execution (requires Memoria) ─────────────────────────────────────


from api.database import SessionLocal  # noqa: E402


@pytest.fixture()
def db_factory():
    return SessionLocal


@pytest.fixture(autouse=True)
def _setup_memoria_env():
    """Set Memoria environment variables from test configuration."""
    os.environ["MEMORIA_BASE_URL"] = os.environ.get("TEST_MEMORIA_BASE_URL", "http://localhost:8100")
    os.environ["MEMORIA_MASTER_KEY"] = os.environ.get("TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
    os.environ["MEMORIA_API_KEY"] = os.environ.get("TEST_MEMORIA_API_KEY", "")
    yield


@pytest.fixture()
def memoria_client(_setup_memoria_env):
    """Create Memoria HTTP client for verification."""
    from core.memory.backends.memoria_http import MemoriaHTTPClient
    return MemoriaHTTPClient(
        base_url=os.environ["MEMORIA_BASE_URL"],
        master_key=os.environ["MEMORIA_MASTER_KEY"],
        api_key=os.environ["MEMORIA_API_KEY"] or None,
    )


@pytest.fixture()
def editor(db_factory, _setup_memoria_env):
    """Create editor with test Memoria configuration."""
    from core.memory.factory import create_editor
    return create_editor(db_factory)


@pytest.fixture()
def programmer(editor, db_factory, _setup_memoria_env):
    return MemoryProgrammer(editor, db_factory)


@pytest.fixture(autouse=True)
def _cleanup(db_factory, memoria_client):
    """Clean up test data after each test."""
    yield
    # Cleanup is handled by Memoria's user isolation


class TestInjectAction:
    def test_inject_creates_memory(self, programmer, memoria_client):
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [{"inject": {"content": "Python prefers spaces over tabs", "type": "semantic"}}],
        )
        assert result.actions_executed == 1
        assert result.actions_failed == 0
        assert result.results[0].success is True
        memory_id = result.results[0].detail["memory_id"]

        # Verify via Memoria API
        mem = memoria_client.get_memory(uid, memory_id)
        assert mem is not None
        assert mem["content"] == "Python prefers spaces over tabs"
        assert mem["memory_type"] == "semantic"

    def test_inject_missing_content_fails(self, programmer):
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [{"inject": {"type": "semantic"}}],
        )
        assert result.actions_failed == 1
        assert result.results[0].success is False
        assert "content" in result.results[0].error


class TestCorrectAction:
    def test_correct_supersedes_memory(self, programmer, memoria_client):
        uid = f"test_prog_{generate_id()}"
        # First inject
        r1 = programmer.execute(
            uid,
            [{"inject": {"content": "Earth is flat", "type": "semantic"}}],
        )
        old_id = r1.results[0].detail["memory_id"]

        # Then correct
        r2 = programmer.execute(
            uid,
            [{"correct": {"memory_id": old_id, "new_content": "Earth is round"}}],
        )
        assert r2.actions_executed == 1
        new_id = r2.results[0].detail["new_id"]
        assert new_id != old_id

        # Verify via Memoria API - old memory should be superseded
        old_mem = memoria_client.get_memory(uid, old_id)
        assert old_mem is None  # Corrected memories are deactivated

        # New memory should exist
        new_mem = memoria_client.get_memory(uid, new_id)
        assert new_mem is not None
        assert new_mem["content"] == "Earth is round"

    def test_correct_missing_fields_fails(self, programmer):
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [{"correct": {"memory_id": "nonexistent"}}],
        )
        assert result.results[0].success is False
        assert "new_content" in result.results[0].error


class TestPurgeAction:
    def test_purge_by_type(self, programmer, memoria_client):
        uid = f"test_prog_{generate_id()}"
        # Inject two memories
        programmer.execute(
            uid,
            [
                {"inject": {"content": "fact 1", "type": "semantic"}},
                {"inject": {"content": "fact 2", "type": "semantic"}},
            ],
        )

        # Verify both exist
        memories = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(memories.get("items", [])) == 2

        # Purge all semantic
        result = programmer.execute(
            uid,
            [{"purge": {"filter": {"type": "semantic"}}}],
        )
        assert result.actions_executed == 1
        assert result.results[0].detail["deactivated"] == 2

        # Verify via Memoria API
        memories = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(memories.get("items", [])) == 0


class TestTuneAction:
    def test_tune_sets_strategy_and_params(self, programmer, db_factory):
        """Tune action sets user strategy and persists validated params."""
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [{"tune": {
                "strategy": "vector:v1",
                "params": {"semantic_weight": 0.6, "temporal_weight": 0.2},
            }}],
        )
        assert result.actions_executed == 1
        assert result.results[0].success is True
        assert result.results[0].detail["strategy"] == "vector:v1"
        # Validated params include defaults for unspecified fields
        validated = result.results[0].detail["params"]
        assert validated["semantic_weight"] == 0.6
        assert validated["temporal_weight"] == 0.2

        # Verify strategy set in DB (use ORM to get proper JSON deserialization)
        from core.memory.models.memory_config import MemoryUserConfig

        with db_factory() as db:
            row = db.query(MemoryUserConfig).filter_by(user_id=uid).first()
            assert row is not None
            assert row.strategy_key == "vector:v1"
            assert row.params_json is not None
            assert row.params_json["semantic_weight"] == 0.6

    def test_tune_missing_strategy_fails(self, programmer):
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [{"tune": {"params": {"semantic_weight": 0.5}}}],
        )
        assert result.results[0].success is False
        assert "strategy" in result.results[0].error


class TestMultiActionScript:
    def test_multi_action_yaml_workflow(self, programmer, memoria_client):
        """Full workflow: inject two memories, correct one, purge the other."""
        uid = f"test_prog_{generate_id()}"

        # Step 1: inject two
        r1 = programmer.execute(
            uid,
            [
                {"inject": {"content": "memory A", "type": "semantic"}},
                {"inject": {"content": "memory B", "type": "procedural"}},
            ],
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
        r2 = programmer.execute(uid, yaml_script)
        assert r2.actions_executed == 2
        assert r2.actions_failed == 0

        # Verify via Memoria API
        # A should be corrected (old deactivated, new created)
        # B should be purged
        semantic_mems = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(semantic_mems.get("items", [])) == 1
        assert semantic_mems["items"][0]["content"] == "memory A corrected"

        procedural_mems = memoria_client.list_memories(uid, memory_type="procedural")
        assert len(procedural_mems.get("items", [])) == 0

    def test_partial_failure_continues(self, programmer):
        """With atomic=False, if one action fails, others still execute."""
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [
                {"inject": {"content": "good one"}},
                {"correct": {"memory_id": "nonexistent_id", "new_content": "x"}},
                {"inject": {"content": "another good one"}},
            ],
            atomic=False,
        )
        assert result.actions_executed == 2
        assert result.actions_failed == 1
        assert result.results[0].success is True
        assert result.results[1].success is False
        assert result.results[2].success is True


class TestBatchInject:
    def test_consecutive_injects_batched(self, programmer, memoria_client):
        """Multiple consecutive injects are coalesced into one batch INSERT."""
        uid = f"test_prog_{generate_id()}"
        n = 20
        actions = [{"inject": {"content": f"fact {i}", "type": "semantic"}} for i in range(n)]
        result = programmer.execute(uid, actions)

        # All coalesced into 1 batch action
        assert len(result.results) == 1
        assert result.results[0].detail["count"] == n
        assert result.actions_failed == 0

        # All 20 in Memoria
        memories = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(memories.get("items", [])) == n

    def test_non_consecutive_injects_not_batched(self, programmer):
        """Injects separated by other actions are not coalesced."""
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [
                {"inject": {"content": "a"}},
                {"purge": {"filter": {"type": "working"}}},
                {"inject": {"content": "b"}},
            ],
        )
        # 3 separate actions: inject, purge, inject
        assert len(result.results) == 3


class TestTimeout:
    def test_timeout_raises(self, programmer):
        """Timeout raises ProgramTimeoutError."""
        uid = f"test_timeout_{generate_id()}"
        actions = [{"purge": {"filter": {"type": "working"}}}]
        with pytest.raises(ProgramTimeoutError):
            programmer.execute(uid, actions, timeout_seconds=-1)


class TestAtomicExecution:
    def test_atomic_stops_on_first_failure(self, programmer):
        """atomic=True stops executing after first failure."""
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [
                {"correct": {"memory_id": "bad_id", "new_content": "x"}},
                {"inject": {"content": "should not run"}},
            ],
            atomic=True,
        )
        # Only 1 result — second action was skipped
        assert len(result.results) == 1
        assert result.results[0].success is False

    def test_non_atomic_continues_after_failure(self, programmer):
        """atomic=False runs all actions regardless of failures."""
        uid = f"test_prog_{generate_id()}"
        result = programmer.execute(
            uid,
            [
                {"correct": {"memory_id": "bad_id", "new_content": "x"}},
                {"inject": {"content": "runs anyway"}},
            ],
            atomic=False,
        )
        assert len(result.results) == 2
        assert result.results[0].success is False
        assert result.results[1].success is True
