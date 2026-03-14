"""End-to-end tests for MemoryProgramTool (CLI layer).

Verifies the full loop: tool execute → programmer → editor → Memoria API.
"""

import asyncio
import json
import os

import pytest

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")

from cli.tools.memory_program import MemoryProgramTool
from core.utils.id_generator import generate_id


@pytest.fixture(autouse=True)
def _setup_memoria_env():
    """Set Memoria environment variables from test configuration."""
    os.environ["MEMORIA_BASE_URL"] = os.environ.get("TEST_MEMORIA_BASE_URL", "http://localhost:8100")
    os.environ["MEMORIA_MASTER_KEY"] = os.environ.get("TEST_MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
    os.environ["MEMORIA_API_KEY"] = os.environ.get("TEST_MEMORIA_API_KEY", "")
    yield


@pytest.fixture()
def tool():
    return MemoryProgramTool()


@pytest.fixture()
def uid():
    return f"test_cli_{generate_id()}"


@pytest.fixture()
def memoria_client():
    """Create Memoria HTTP client for verification."""
    from core.memory.backends.memoria_http import MemoriaHTTPClient
    return MemoriaHTTPClient(
        base_url=os.environ["MEMORIA_BASE_URL"],
        master_key=os.environ["MEMORIA_MASTER_KEY"],
        api_key=os.environ["MEMORIA_API_KEY"] or None,
    )


def _run(coro):
    return asyncio.run(coro)


def _call(tool, **kwargs):
    return json.loads(_run(tool.execute(**kwargs)))


class TestInjectE2E:
    def test_inject_single_all_fields(self, tool, uid, memoria_client):
        """Inject one memory → verify via Memoria API."""
        result = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "Python uses 4-space indent", "type": "semantic", "trust": "T1"}}],
            explain=True,
        )

        assert result["actions_executed"] == 1, f"result={result}"
        assert result["actions_failed"] == 0
        assert len(result["explain"]) == 1
        exp = result["explain"][0]
        assert exp["action"] == "inject"
        assert exp["success"] is True
        mid = exp["detail"]["memory_id"]
        assert len(mid) == 32

        # Verify via Memoria API
        mem = memoria_client.get_memory(uid, mid)
        assert mem is not None
        assert mem["content"] == "Python uses 4-space indent"
        assert mem["memory_type"] == "semantic"

    def test_inject_with_trust_t2(self, tool, uid, memoria_client):
        """Inject with T2 trust → verify via Memoria API."""
        result = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "fact", "type": "procedural", "trust": "T2"}}],
            explain=True,
        )
        mid = result["explain"][0]["detail"]["memory_id"]

        mem = memoria_client.get_memory(uid, mid)
        assert mem["memory_type"] == "procedural"


class TestCorrectE2E:
    def test_correct_full_lifecycle(self, tool, uid, memoria_client):
        """Inject → correct → verify old deactivated, new created."""
        # Step 1: inject
        r1 = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "Earth is flat", "type": "semantic"}}],
            explain=True,
        )
        old_mid = r1["explain"][0]["detail"]["memory_id"]

        # Step 2: correct
        r2 = _call(
            tool,
            user_id=uid,
            actions=[{"correct": {"memory_id": old_mid, "new_content": "Earth is round", "reason": "factual error"}}],
            explain=True,
        )
        assert r2["actions_executed"] == 1
        assert r2["actions_failed"] == 0
        new_mid = r2["explain"][0]["detail"]["new_id"]
        assert r2["explain"][0]["detail"]["old_id"] == old_mid

        # Verify new memory exists
        new_mem = memoria_client.get_memory(uid, new_mid)
        assert new_mem is not None
        assert new_mem["content"] == "Earth is round"


class TestPurgeE2E:
    def test_purge_by_type(self, tool, uid, memoria_client):
        """Inject 2 semantic + 1 procedural → purge semantic → verify via API."""
        _call(tool, user_id=uid, actions=[
            {"inject": {"content": "sem1", "type": "semantic"}},
            {"inject": {"content": "sem2", "type": "semantic"}},
            {"inject": {"content": "proc1", "type": "procedural"}},
        ], explain=True)

        # Verify 3 memories exist
        all_mems = memoria_client.list_memories(uid)
        assert len(all_mems.get("items", [])) == 3

        r = _call(
            tool,
            user_id=uid,
            actions=[{"purge": {"filter": {"type": "semantic"}, "reason": "cleanup"}}],
            explain=True,
        )
        assert r["actions_executed"] == 1
        detail = r["explain"][0]["detail"]
        assert detail["deactivated"] == 2

        # Verify only procedural remains
        semantic_mems = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(semantic_mems.get("items", [])) == 0

        procedural_mems = memoria_client.list_memories(uid, memory_type="procedural")
        assert len(procedural_mems.get("items", [])) == 1


class TestTuneE2E:
    def test_tune_sets_strategy_and_params(self, tool, uid):
        """Tune action → verify success."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"tune": {"strategy": "balanced", "params": {"vector_weight": 0.6}}}],
            explain=True,
        )
        assert r["actions_executed"] == 1
        assert r["explain"][0]["detail"]["strategy"] == "balanced"


class TestDryRunE2E:
    def test_dry_run_no_writes(self, tool, uid, memoria_client):
        """Dry-run → no data written."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "should not persist"}},
            ],
            dry_run=True,
        )
        assert r["dry_run"] is True
        assert r["actions_executed"] == 0

        # Verify no memories
        mems = memoria_client.list_memories(uid)
        assert len(mems.get("items", [])) == 0


class TestExplainOutput:
    def test_explain_true_returns_detail(self, tool, uid):
        """explain=True → each result has action, success, detail keys."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"inject": {"content": "x"}}, {"inject": {"content": "y"}}],
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
            explain=False,
        )
        assert "results" in r
        assert "explain" not in r
        for item in r["results"]:
            assert "action" in item
            assert "success" in item
            assert "detail" not in item


class TestErrorHandling:
    def test_correct_nonexistent_memory(self, tool, uid):
        """Correct a non-existent memory_id → action fails, no crash."""
        r = _call(
            tool,
            user_id=uid,
            actions=[{"correct": {"memory_id": "nonexistent", "new_content": "x"}}],
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
            explain=True,
        )
        assert r["actions_failed"] == 1
        assert r["explain"][0]["success"] is False


class TestMultiActionE2E:
    def test_inject_then_purge_final_state(self, tool, uid, memoria_client):
        """Inject 3 → purge all semantic → verify final state."""
        r = _call(
            tool,
            user_id=uid,
            actions=[
                {"inject": {"content": "a", "type": "semantic"}},
                {"inject": {"content": "b", "type": "semantic"}},
                {"inject": {"content": "c", "type": "procedural"}},
                {"purge": {"filter": {"type": "semantic"}}},
            ],
            explain=True,
        )
        assert r["actions_failed"] == 0
        assert r["actions_executed"] == 2  # batch_inject + purge

        # Final state: only procedural remains
        semantic_mems = memoria_client.list_memories(uid, memory_type="semantic")
        assert len(semantic_mems.get("items", [])) == 0

        procedural_mems = memoria_client.list_memories(uid, memory_type="procedural")
        assert len(procedural_mems.get("items", [])) == 1
